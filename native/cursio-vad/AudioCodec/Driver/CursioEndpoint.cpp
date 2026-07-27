/*++

Cursio Virtual Audio Driver — dynamic endpoint devices.

NEW Cursio code (not from the Microsoft sample). See CursioEndpoint.h.

This first cut creates the child PDO and runs the ACX device init sequence so a
device *appears*; the render+capture circuits are added in the next step. A tiny
fixed-size registry maps endpoint id -> child WDFDEVICE so RemoveEndpoint can
find and delete it.

Environment: Kernel mode

--*/

#include "public.h"         // WDF/ACX/initguid (include first, once)
#include "CursioEndpoint.h"
#include "CursioStreamHook.h" // CURSIO_INVALID_ENDPOINT, stream-side entry points
#include "DriverSettings.h"  // CODEC_RENDER/CAPTURE_COMPONENT_GUID, MIC_CUSTOM_NAME

//
// Forward-declare the sample's circuit builders (they live in SamplesCommon.lib;
// their full prototypes are in Common/Private.h, which we do NOT include here
// because it re-includes <initguid.h> and would clash with public.h in this TU).
// Signatures copied verbatim from Common/Private.h.
//
extern "C" {
NTSTATUS
CodecR_CreateRenderCircuit(
    _In_  WDFDEVICE              Device,
    _In_  const GUID*            ComponentGuid,
    _In_  const UNICODE_STRING*  CircuitName,
    _Out_ ACXCIRCUIT*            Circuit
);

NTSTATUS
CodecC_CreateCaptureCircuit(
    _In_  WDFDEVICE              Device,
    _In_  const GUID*            ComponentGuid,
    _In_  const GUID*            MicCustomName,
    _In_  const UNICODE_STRING*  CircuitName,
    _Out_ ACXCIRCUIT*            Circuit
);
}

// Local bail-on-failure helper (see CursioBridge.cpp for why not the sample's).
#define CURSIO_RETURN_IF_FAILED(expr)                 \
    do {                                              \
        NTSTATUS _st = (expr);                        \
        if (!NT_SUCCESS(_st)) { return _st; }         \
    } while (0)

//
// The parent bus device (the sample's Codec device). Set once at BusDeviceAdd.
// Child endpoint PDOs are created under it.
//
static WDFDEVICE g_CursioParentDevice = nullptr;

//
// Endpoint registry. A small fixed table keeps this lock-light and allocation-
// free on the hot path; the peer realistically shares a handful of endpoints,
// not thousands. Guarded by a fast mutex since ADD/REMOVE arrive on IOCTLs that
// run at PASSIVE_LEVEL but can interleave.
//
#define CURSIO_MAX_ENDPOINTS 64

typedef struct _CURSIO_ENDPOINT_ENTRY {
    BOOLEAN             InUse;
    CURSIO_ENDPOINT_ID  Id;
    WDFDEVICE           ChildDevice;
    CURSIO_RING         RenderRing;   // driver -> user mode (PULL_RENDER)
    CURSIO_RING         CaptureRing;  // user mode -> driver (PUSH_CAPTURE)
} CURSIO_ENDPOINT_ENTRY;

static CURSIO_ENDPOINT_ENTRY g_Endpoints[CURSIO_MAX_ENDPOINTS];
static FAST_MUTEX            g_EndpointsLock;
static BOOLEAN               g_EndpointsLockInit = FALSE;

static VOID
Cursio_EnsureLock()
{
    // DriverEntry path is single-threaded before any IOCTL can arrive, so this
    // lazy init is safe: the first Set/Create happens before concurrent IOCTLs.
    if (!g_EndpointsLockInit) {
        ExInitializeFastMutex(&g_EndpointsLock);
        g_EndpointsLockInit = TRUE;
    }
}

_Use_decl_annotations_
VOID
Cursio_SetParentDevice(
    WDFDEVICE Parent
)
{
    Cursio_EnsureLock();
    g_CursioParentDevice = Parent;
    DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_ERROR_LEVEL,
               "Cursio: parent bus device registered (%p)\n", Parent);
}

//
// Find a free slot / an existing id. Caller holds g_EndpointsLock.
//
static CURSIO_ENDPOINT_ENTRY*
Cursio_FindSlot(
    _In_ CURSIO_ENDPOINT_ID Id,
    _In_ BOOLEAN AllocateIfMissing
)
{
    CURSIO_ENDPOINT_ENTRY* freeSlot = nullptr;
    for (ULONG i = 0; i < CURSIO_MAX_ENDPOINTS; ++i) {
        if (g_Endpoints[i].InUse && g_Endpoints[i].Id == Id) {
            return &g_Endpoints[i];
        }
        if (!g_Endpoints[i].InUse && freeSlot == nullptr) {
            freeSlot = &g_Endpoints[i];
        }
    }
    return AllocateIfMissing ? freeSlot : nullptr;
}

_Use_decl_annotations_
NTSTATUS
Cursio_CreateEndpoint(
    const CURSIO_ENDPOINT_DESC* Desc
)
{
    if (g_CursioParentDevice == nullptr) {
        // No parent bus yet — BusDeviceAdd hasn't run. Shouldn't happen once the
        // device is up, but fail cleanly rather than dereferencing null.
        //
        // This is the ONLY other source of STATUS_DEVICE_NOT_READY in the bridge,
        // so if user mode reports 0x80070016 from ADD_ENDPOINT and the handshake
        // gate is gone, it came from here.
        DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_ERROR_LEVEL,
                   "Cursio: ADD_ENDPOINT refused - no parent bus device yet "
                   "(Codec_EvtBusDeviceAdd has not run or failed before SetParentDevice)\n");
        // Distinct from every other failure here, so user mode can tell "the bus
        // isn't up" apart from a bad descriptor without needing a kernel
        // debugger — DbgPrint alone is invisible unless DebugView is running.
        return STATUS_NO_SUCH_DEVICE;
    }

    Cursio_EnsureLock();

    // Reserve a registry slot up front so a duplicate id is rejected before we
    // spend effort creating a device.
    ExAcquireFastMutex(&g_EndpointsLock);
    CURSIO_ENDPOINT_ENTRY* slot = Cursio_FindSlot(Desc->Id, TRUE);
    BOOLEAN duplicate = (slot != nullptr && slot->InUse && slot->Id == Desc->Id);
    if (slot == nullptr || duplicate) {
        ExReleaseFastMutex(&g_EndpointsLock);
        return duplicate ? STATUS_OBJECT_NAME_COLLISION : STATUS_INSUFFICIENT_RESOURCES;
    }
    // Mark reserved (Id set, device filled in on success) so a concurrent create
    // for the same id can't grab the same slot.
    slot->InUse = TRUE;
    slot->Id = Desc->Id;
    slot->ChildDevice = nullptr;
    ExReleaseFastMutex(&g_EndpointsLock);

    NTSTATUS status = STATUS_SUCCESS;

    // Undo the slot reservation if anything below fails.
    auto slotGuard = [&]() {
        if (!NT_SUCCESS(status)) {
            ExAcquireFastMutex(&g_EndpointsLock);
            Cursio_RingFree(&slot->RenderRing);
            Cursio_RingFree(&slot->CaptureRing);
            slot->InUse = FALSE;
            slot->ChildDevice = nullptr;
            ExReleaseFastMutex(&g_EndpointsLock);
        }
    };

    // Rings must exist before the device appears: the moment PnP publishes the
    // endpoint the audio engine may open a stream, and its first packet has to
    // find a ring rather than a half-built endpoint.
    status = Cursio_RingInit(&slot->RenderRing);
    if (NT_SUCCESS(status)) {
        status = Cursio_RingInit(&slot->CaptureRing);
    }
    if (!NT_SUCCESS(status)) {
        slotGuard();
        return status;
    }

    // --- Child PDO device-init ---
    PWDFDEVICE_INIT deviceInit = WdfPdoInitAllocate(g_CursioParentDevice);
    if (deviceInit == nullptr) {
        status = STATUS_INSUFFICIENT_RESOURCES;
        slotGuard();
        return status;
    }

    // A stable, id-scoped hardware id and instance id so PnP can match/replace
    // this endpoint deterministically across create/remove cycles.
    DECLARE_UNICODE_STRING_SIZE(hardwareId, 64);
    DECLARE_UNICODE_STRING_SIZE(instanceId, 32);
    status = RtlUnicodeStringPrintf(&hardwareId, L"Cursio\\Audio&Endpoint_%u", Desc->Id);
    if (NT_SUCCESS(status)) {
        status = WdfPdoInitAssignDeviceID(deviceInit, &hardwareId);
    }
    if (NT_SUCCESS(status)) {
        status = WdfPdoInitAddHardwareID(deviceInit, &hardwareId);
    }
    if (NT_SUCCESS(status)) {
        status = RtlUnicodeStringPrintf(&instanceId, L"%u", Desc->Id);
    }
    if (NT_SUCCESS(status)) {
        status = WdfPdoInitAssignInstanceID(deviceInit, &instanceId);
    }
    if (!NT_SUCCESS(status)) {
        WdfDeviceInitFree(deviceInit);
        slotGuard();
        return status;
    }

    // --- ACX device init (mirrors the sample's Codec_EvtBusDeviceAdd) ---
    ACX_DEVICEINIT_CONFIG devInitCfg;
    ACX_DEVICEINIT_CONFIG_INIT(&devInitCfg);
    status = AcxDeviceInitInitialize(deviceInit, &devInitCfg);
    if (!NT_SUCCESS(status)) {
        WdfDeviceInitFree(deviceInit);
        slotGuard();
        return status;
    }

    WDFDEVICE childDevice = nullptr;
    status = WdfDeviceCreate(&deviceInit, WDF_NO_OBJECT_ATTRIBUTES, &childDevice);
    if (!NT_SUCCESS(status)) {
        // WdfDeviceCreate frees deviceInit itself on failure; don't double-free.
        slotGuard();
        return status;
    }

    ACX_DEVICE_CONFIG devCfg;
    ACX_DEVICE_CONFIG_INIT(&devCfg);
    status = AcxDeviceInitialize(childDevice, &devCfg);
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(childDevice);
        slotGuard();
        return status;
    }

    // --- Render + capture circuits (reusing the sample's builders) ---
    //
    // Each endpoint carries both directions: a render circuit (a Windows OUTPUT
    // = the peer's speaker) and a capture circuit (a Windows INPUT = the peer's
    // source). Circuit names are made id-unique so multiple endpoints don't
    // collide on the same internal name.
    //
    DECLARE_UNICODE_STRING_SIZE(renderName, 48);
    DECLARE_UNICODE_STRING_SIZE(captureName, 48);
    status = RtlUnicodeStringPrintf(&renderName, L"Cursio Render %u", Desc->Id);
    if (NT_SUCCESS(status)) {
        status = RtlUnicodeStringPrintf(&captureName, L"Cursio Capture %u", Desc->Id);
    }
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(childDevice);
        slotGuard();
        return status;
    }

    // Publish the device into the registry BEFORE creating circuits: circuit
    // creation (and the PnP registration below) can synchronously call back into
    // EvtCircuitCreateStream, and that callback resolves its endpoint id by
    // looking this device up. Publishing afterwards would leave those streams
    // unable to find their rings.
    ExAcquireFastMutex(&g_EndpointsLock);
    slot->ChildDevice = childDevice;
    ExReleaseFastMutex(&g_EndpointsLock);

    ACXCIRCUIT renderCircuit = nullptr;
    status = CodecR_CreateRenderCircuit(
        childDevice, &CODEC_RENDER_COMPONENT_GUID, &renderName, &renderCircuit);
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(childDevice);
        slotGuard();
        return status;
    }

    ACXCIRCUIT captureCircuit = nullptr;
    status = CodecC_CreateCaptureCircuit(
        childDevice, &CODEC_CAPTURE_COMPONENT_GUID, &MIC_CUSTOM_NAME, &captureName, &captureCircuit);
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(childDevice);
        slotGuard();
        return status;
    }

    // Add both circuits TO the child device (AcxDeviceAddCircuit takes a
    // circuit), then register the whole child device UNDER the parent bus
    // (AcxDeviceAddCircuitDevice takes ParentDevice, ChildDevice) — that last
    // call is what makes the endpoint appear; PnP takes over from here.
    status = AcxDeviceAddCircuit(childDevice, renderCircuit);
    if (NT_SUCCESS(status)) {
        status = AcxDeviceAddCircuit(childDevice, captureCircuit);
    }
    if (NT_SUCCESS(status)) {
        status = AcxDeviceAddCircuitDevice(g_CursioParentDevice, childDevice);
    }
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(childDevice);  // tears the circuits down with it
        slotGuard();
        return status;
    }

    // (ChildDevice was published into the registry before circuit creation.)
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
NTSTATUS
Cursio_RemoveEndpoint(
    CURSIO_ENDPOINT_ID Id
)
{
    Cursio_EnsureLock();

    WDFDEVICE toDelete = nullptr;
    ExAcquireFastMutex(&g_EndpointsLock);
    CURSIO_ENDPOINT_ENTRY* slot = Cursio_FindSlot(Id, FALSE);
    if (slot != nullptr) {
        toDelete = slot->ChildDevice;
        // Clearing InUse first makes the entry invisible to new lookups, so no
        // stream can start using these rings while we are tearing them down.
        slot->InUse = FALSE;
        slot->ChildDevice = nullptr;
    }
    ExReleaseFastMutex(&g_EndpointsLock);

    if (slot == nullptr) {
        return STATUS_NOT_FOUND;
    }

    // Deleting the child device object tears down its ACX circuits with it.
    // (ACX fails this if a stream is in RUN state; the caller retries or the
    // teardown waits for the OS to stop the stream.)
    if (toDelete != nullptr) {
        WdfObjectDelete(toDelete);
    }

    // Only now free the rings. WdfObjectDelete above has stopped the streams, so
    // no packet callback can still be holding these buffers — freeing earlier
    // would race a running stream against freed non-paged pool.
    ExAcquireFastMutex(&g_EndpointsLock);
    Cursio_RingFree(&slot->RenderRing);
    Cursio_RingFree(&slot->CaptureRing);
    ExReleaseFastMutex(&g_EndpointsLock);

    return STATUS_SUCCESS;
}

//
// --- Ring / device lookup ---
//
// These run on the streaming path, so they take the registry lock only long
// enough to resolve the entry. The rings themselves are internally locked.
//

static CURSIO_RING*
Cursio_GetRing(
    _In_ CURSIO_ENDPOINT_ID Id,
    _In_ BOOLEAN Render
)
{
    if (!g_EndpointsLockInit) {
        return nullptr;
    }

    ExAcquireFastMutex(&g_EndpointsLock);
    CURSIO_ENDPOINT_ENTRY* slot = Cursio_FindSlot(Id, FALSE);
    CURSIO_RING* ring = nullptr;
    if (slot != nullptr && slot->InUse) {
        ring = Render ? &slot->RenderRing : &slot->CaptureRing;
    }
    ExReleaseFastMutex(&g_EndpointsLock);

    return ring;
}

_Use_decl_annotations_
CURSIO_RING*
Cursio_GetRenderRing(
    CURSIO_ENDPOINT_ID Id
)
{
    return Cursio_GetRing(Id, TRUE);
}

_Use_decl_annotations_
CURSIO_RING*
Cursio_GetCaptureRing(
    CURSIO_ENDPOINT_ID Id
)
{
    return Cursio_GetRing(Id, FALSE);
}

//
// --- Stream-side entry points (CursioStreamHook.h) ---
//
// Called from the sample's ProcessPacket at DISPATCH_LEVEL. Both re-resolve the
// ring per packet: an endpoint can be removed between packets, and re-resolving
// turns that into a clean no-op instead of a use-after-free.
//

_Use_decl_annotations_
VOID
Cursio_StreamPushRender(
    CURSIO_ENDPOINT_ID Id,
    const UCHAR* Data,
    ULONG Bytes
)
{
    if (Id == CURSIO_INVALID_ENDPOINT) {
        return;
    }

    CURSIO_RING* ring = Cursio_GetRenderRing(Id);
    if (ring != nullptr) {
        Cursio_RingWrite(ring, Data, Bytes);
    }
    // No ring => endpoint went away mid-stream; drop the packet. The stream is
    // about to be stopped by the teardown anyway.
}

_Use_decl_annotations_
ULONG
Cursio_StreamPullCapture(
    CURSIO_ENDPOINT_ID Id,
    UCHAR* Data,
    ULONG Bytes
)
{
    if (Id == CURSIO_INVALID_ENDPOINT) {
        // Not ours — leave the buffer alone so the caller runs its own fallback.
        return 0;
    }

    CURSIO_RING* ring = Cursio_GetCaptureRing(Id);
    if (ring == nullptr) {
        // Ours, but torn down: emit silence rather than the sample's test tone.
        RtlZeroMemory(Data, Bytes);
        return 0;
    }

    return Cursio_RingRead(ring, Data, Bytes);
}

_Use_decl_annotations_
CURSIO_ENDPOINT_ID
Cursio_LookupEndpointByDevice(
    WDFDEVICE Device
)
{
    if (!g_EndpointsLockInit || Device == nullptr) {
        return CURSIO_INVALID_ENDPOINT;
    }

    ExAcquireFastMutex(&g_EndpointsLock);
    CURSIO_ENDPOINT_ID id = CURSIO_INVALID_ENDPOINT;
    for (ULONG i = 0; i < CURSIO_MAX_ENDPOINTS; ++i) {
        if (g_Endpoints[i].InUse && g_Endpoints[i].ChildDevice == Device) {
            id = g_Endpoints[i].Id;
            break;
        }
    }
    ExReleaseFastMutex(&g_EndpointsLock);

    return id;
}
