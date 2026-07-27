/*++

Cursio Virtual Audio Driver — user-mode bridge (control device + IOCTL).

NEW Cursio code (not from the Microsoft sample). See CursioBridge.h and the
shared contract in Shared/CursioIoctl.h.

This file stands up a WDF *control* device (a non-PnP device with a fixed name,
so a user-mode process can open it without a device-interface GUID) and an IOCTL
queue. The IOCTL handler is the seam between Cursio user mode and the ACX
streams: it checks protocol versions, creates/destroys peer endpoints, and moves
PCM in (capture) and out (render).

PCM routing is live: ADD_ENDPOINT creates a dynamic render+capture device pair
(CursioEndpoint.cpp), and PUSH_CAPTURE / PULL_RENDER move samples through that
endpoint's rings (CursioRing.cpp), which the ACX stream engines drain and fill on
their timer callbacks. Both PCM IOCTLs are non-blocking; the rings absorb the
jitter between the network loop and the audio clock.

Environment: Kernel mode

--*/

#include "public.h"         // WDF/ACX/initguid + INIT_CODE_SEG (include first, once)
#include "CursioBridge.h"
#include "CursioEndpoint.h" // dynamic endpoint create/remove
#include "CursioIoctl.h"
#include "cpp_utils.h"      // scope_exit
#include <wdmsec.h>         // SDDL_DEVOBJ_SYS_ALL_ADM_ALL

// NOTE: no WPP (.tmh) here on purpose — this file doesn't emit WPP trace calls,
// so it stays independent of the sample's WPP scan configuration. Add WPP later
// if the bridge needs tracing.

// Local "bail on failure" helper, so this file doesn't depend on the sample's
// RETURN_NTSTATUS_IF_FAILED macro (which comes in through the WPP/trace include
// chain we deliberately skip here).
#define CURSIO_RETURN_IF_FAILED(expr)                 \
    do {                                              \
        NTSTATUS _st = (expr);                        \
        if (!NT_SUCCESS(_st)) { return _st; }         \
    } while (0)

//
// Kernel-side tracing for the bridge, visible in DebugView (or WinDbg) with no
// WPP setup — deliberately plain, because this layer is driven from user mode
// and its failures ("device not ready", "invalid parameter") are otherwise
// indistinguishable from the outside: user mode only ever sees an NTSTATUS.
//
#define CURSIO_LOG(fmt, ...) \
    DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_ERROR_LEVEL, "Cursio: " fmt "\n", __VA_ARGS__)

//
// The control device, once created. A driver has at most one; we keep it in a
// file-scope global because the control device outlives any single PnP device
// and is addressed by name, not by handle.
//
static WDFDEVICE g_CursioControlDevice = nullptr;

//
// Whether a compatible HELLO has been seen since the driver loaded.
//
// NOT a per-connection gate. It used to be one — every other IOCTL returned
// STATUS_DEVICE_NOT_READY until HELLO had run — but that is wrong here: the
// user-mode side opens a SEPARATE handle per endpoint (one per VadSource /
// VadSink), so "has this handle shaken hands?" is not something a single global
// flag can answer. A driver-wide flag makes them interfere: the first handle's
// HELLO satisfies later handles, and one failed HELLO disarms all of them. That
// mismatch is what made ADD_ENDPOINT fail with 0x80070016 (ERROR_NOT_READY)
// even though the user side had just handshaked successfully.
//
// The version check itself still matters, so HELLO still rejects a mismatched
// build — but it now guards only itself. Refusing work because of *another*
// handle's state was never a real safety property; a genuinely incompatible
// build fails at HELLO and never gets as far as sending PCM.
//
static BOOLEAN g_CursioSeenCompatibleHello = FALSE;

//
// Sequence number stamped on PULL_RENDER replies, so the user-mode side can
// detect gaps the same way it does for network AudioChunks. Bumped under the
// IOCTL queue's serialization, so no interlocked access is needed.
//
static unsigned int g_CursioRenderSeq = 0;

//
// Forward declarations.
//
extern "C" {
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL Cursio_EvtIoDeviceControl;
}

//
// Handle IOCTL_CURSIO_HELLO: check the caller's protocol version and reply with
// ours. A mismatch fails the request, so the user-mode side learns immediately
// instead of sending audio a stale driver would misparse. It gates nothing else
// — see `g_CursioSeenCompatibleHello` for why a driver-wide gate was wrong.
//
static NTSTATUS
Cursio_HandleHello(
    _In_ WDFREQUEST Request,
    _Out_ size_t* BytesReturned
)
{
    *BytesReturned = 0;

    unsigned int* inVersion = nullptr;
    size_t inLen = 0;
    CURSIO_RETURN_IF_FAILED(WdfRequestRetrieveInputBuffer(
        Request, sizeof(*inVersion), reinterpret_cast<PVOID*>(&inVersion), &inLen));

    // Read the caller's version BEFORE touching the output buffer.
    //
    // METHOD_BUFFERED gives input and output the SAME system buffer (sized
    // max(inLen, outLen)). Writing the reply first therefore overwrites the very
    // bytes we are about to validate — here both sides are 4 bytes, so the reply
    // would land exactly on top of the incoming version.
    const unsigned int callerVersion = *inVersion;

    CURSIO_HELLO_REPLY* reply = nullptr;
    size_t outLen = 0;
    CURSIO_RETURN_IF_FAILED(WdfRequestRetrieveOutputBuffer(
        Request, sizeof(*reply), reinterpret_cast<PVOID*>(&reply), &outLen));

    reply->DriverProtocolVersion = CURSIO_PROTOCOL_VERSION;
    *BytesReturned = sizeof(*reply);

    CURSIO_LOG("HELLO caller=v%u driver=v%u inLen=%Iu outLen=%Iu",
               callerVersion, CURSIO_PROTOCOL_VERSION, inLen, outLen);

    if (callerVersion != CURSIO_PROTOCOL_VERSION) {
        // Reply still carries our version so the caller can log the mismatch,
        // but the request fails and the handshake stays incomplete.
        g_CursioSeenCompatibleHello = FALSE;
        return STATUS_REVISION_MISMATCH;
    }

    g_CursioSeenCompatibleHello = TRUE;
    return STATUS_SUCCESS;
}

//
// Handle IOCTL_CURSIO_ADD_ENDPOINT: validate the descriptor, then (once the
// dynamic-circuit layer is wired) create a render+capture pair for it. For now
// the descriptor is validated and accepted; circuit creation is the next step.
//
static NTSTATUS
Cursio_HandleAddEndpoint(
    _In_ WDFREQUEST Request
)
{

    CURSIO_ENDPOINT_DESC* desc = nullptr;
    size_t len = 0;
    CURSIO_RETURN_IF_FAILED(WdfRequestRetrieveInputBuffer(
        Request, sizeof(*desc), reinterpret_cast<PVOID*>(&desc), &len));

    // Defend against a non-terminated name from user mode before anyone treats
    // it as a string.
    desc->Name[CURSIO_MAX_NAME_CHARS - 1] = L'\0';

    if (desc->Format.Channels == 0 || desc->Format.SampleRate == 0) {
        return STATUS_INVALID_PARAMETER;
    }

    // Create the child endpoint device (render+capture pair). Circuit wiring is
    // added inside Cursio_CreateEndpoint as it's built out.
    NTSTATUS status = Cursio_CreateEndpoint(desc);
    CURSIO_LOG("ADD_ENDPOINT id=%u rate=%u ch=%u -> 0x%08X",
               desc->Id, desc->Format.SampleRate, desc->Format.Channels, status);
    return status;
}

//
// Handle IOCTL_CURSIO_REMOVE_ENDPOINT.
//
static NTSTATUS
Cursio_HandleRemoveEndpoint(
    _In_ WDFREQUEST Request
)
{
    CURSIO_ENDPOINT_ID_ARG* arg = nullptr;
    size_t len = 0;
    CURSIO_RETURN_IF_FAILED(WdfRequestRetrieveInputBuffer(
        Request, sizeof(*arg), reinterpret_cast<PVOID*>(&arg), &len));

    return Cursio_RemoveEndpoint(arg->Id);
}

//
// Handle IOCTL_CURSIO_PUSH_CAPTURE: PCM arrived from the peer; drop it into the
// endpoint's capture ring, from which the capture stream's RT packets are filled
// so Windows apps hear the peer as a recording source.
//
static NTSTATUS
Cursio_HandlePushCapture(
    _In_ WDFREQUEST Request
)
{
    CURSIO_PCM_CHUNK* chunk = nullptr;
    size_t len = 0;
    CURSIO_RETURN_IF_FAILED(WdfRequestRetrieveInputBuffer(
        Request, sizeof(*chunk), reinterpret_cast<PVOID*>(&chunk), &len));

    // The samples follow the header in the same buffer; make sure the caller's
    // Bytes actually fits what it sent before we read past the buffer.
    if (len < sizeof(*chunk) || chunk->Bytes > (len - sizeof(*chunk))) {
        return STATUS_INVALID_BUFFER_SIZE;
    }
    if (chunk->Bytes == 0) {
        return STATUS_SUCCESS;
    }

    CURSIO_RING* ring = Cursio_GetCaptureRing(chunk->Id);
    if (ring == nullptr) {
        return STATUS_NOT_FOUND;
    }

    Cursio_RingWrite(ring, reinterpret_cast<const UCHAR*>(chunk + 1), chunk->Bytes);
    return STATUS_SUCCESS;
}

//
// Handle IOCTL_CURSIO_PULL_RENDER: drain whatever a Windows app has rendered
// into this endpoint so Cursio can ship it to the peer's speaker.
//
// This is a NON-BLOCKING drain: it returns immediately with however much is
// buffered (possibly zero), rather than pending the request until a packet is
// ready. The user-mode side already runs its own paced audio loop, so it polls
// at the packet rate; a pended/inverted call would add a second timing authority
// for no benefit. The ring absorbs jitter between the two.
//
// Input: a CURSIO_PCM_CHUNK whose Id selects the endpoint.
// Output: a CURSIO_PCM_CHUNK header followed by Bytes of PCM.
//
static NTSTATUS
Cursio_HandlePullRender(
    _In_ WDFREQUEST Request,
    _Out_ size_t*   BytesReturned
)
{
    *BytesReturned = 0;

    // Which endpoint to drain comes in via the input buffer...
    CURSIO_PCM_CHUNK* inChunk = nullptr;
    size_t inLen = 0;
    CURSIO_RETURN_IF_FAILED(WdfRequestRetrieveInputBuffer(
        Request, sizeof(*inChunk), reinterpret_cast<PVOID*>(&inChunk), &inLen));
    CURSIO_ENDPOINT_ID id = inChunk->Id;

    // ...and the PCM goes back out through the output buffer.
    CURSIO_PCM_CHUNK* outChunk = nullptr;
    size_t outLen = 0;
    CURSIO_RETURN_IF_FAILED(WdfRequestRetrieveOutputBuffer(
        Request, sizeof(*outChunk), reinterpret_cast<PVOID*>(&outChunk), &outLen));

    if (outLen <= sizeof(*outChunk)) {
        return STATUS_BUFFER_TOO_SMALL;
    }

    CURSIO_RING* ring = Cursio_GetRenderRing(id);
    if (ring == nullptr) {
        return STATUS_NOT_FOUND;
    }

    // Take at most what the caller's buffer holds, and only real buffered bytes
    // — unlike the streaming path we must NOT zero-pad here, or we would send
    // manufactured silence across the network on every idle poll.
    ULONG capacity = (ULONG)(outLen - sizeof(*outChunk));
    ULONG available = Cursio_RingUsed(ring);
    ULONG toRead = (available < capacity) ? available : capacity;

    ULONG got = 0;
    if (toRead > 0) {
        got = Cursio_RingRead(ring, reinterpret_cast<UCHAR*>(outChunk + 1), toRead);
    }

    outChunk->Id = id;
    outChunk->Seq = g_CursioRenderSeq++;
    outChunk->Bytes = got;
    outChunk->Reserved = 0;

    *BytesReturned = sizeof(*outChunk) + got;
    return STATUS_SUCCESS;
}

//
// The IOCTL dispatch. Runs at PASSIVE_LEVEL (control device, buffered IOCTLs).
//
_Use_decl_annotations_
VOID
Cursio_EvtIoDeviceControl(
    WDFQUEUE   Queue,
    WDFREQUEST Request,
    size_t     OutputBufferLength,
    size_t     InputBufferLength,
    ULONG      IoControlCode
)
{
    UNREFERENCED_PARAMETER(Queue);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);

    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    size_t   bytesReturned = 0;

    switch (IoControlCode) {
    case IOCTL_CURSIO_HELLO:
        status = Cursio_HandleHello(Request, &bytesReturned);
        break;

    case IOCTL_CURSIO_ADD_ENDPOINT:
        status = Cursio_HandleAddEndpoint(Request);
        break;

    case IOCTL_CURSIO_REMOVE_ENDPOINT:
        status = Cursio_HandleRemoveEndpoint(Request);
        break;

    case IOCTL_CURSIO_PUSH_CAPTURE:
        status = Cursio_HandlePushCapture(Request);
        break;

    case IOCTL_CURSIO_PULL_RENDER:
        status = Cursio_HandlePullRender(Request, &bytesReturned);
        break;

    default:
        status = STATUS_INVALID_DEVICE_REQUEST;
        break;
    }

    WdfRequestCompleteWithInformation(Request, status, bytesReturned);
}

//
// Create the control device + its IOCTL queue. Control devices follow a
// specific WDF sequence: allocate a WDFDEVICE_INIT for a control device, name
// it, create it, create a symbolic link so user mode can open it by DOS name,
// add a default queue, then call WdfControlFinishInitializing.
//
_Use_decl_annotations_
INIT_CODE_SEG
NTSTATUS
Cursio_CreateControlDevice(
    WDFDRIVER Driver
)
{
    NTSTATUS status = STATUS_SUCCESS;

    DECLARE_CONST_UNICODE_STRING(deviceName, CURSIO_CONTROL_DEVICE_NAME);
    DECLARE_CONST_UNICODE_STRING(symlinkName, CURSIO_CONTROL_SYMLINK_NAME);

    // Control-device init is allocated from the driver, not a PnP add-device.
    PWDFDEVICE_INIT deviceInit =
        WdfControlDeviceInitAllocate(Driver, &SDDL_DEVOBJ_SYS_ALL_ADM_ALL);
    if (deviceInit == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    // From here, on any failure we must free deviceInit unless WdfDeviceCreate
    // consumed it.
    auto initGuard = scope_exit([&deviceInit]() {
        if (deviceInit != nullptr) {
            WdfDeviceInitFree(deviceInit);
        }
    });

    WdfDeviceInitSetDeviceType(deviceInit, FILE_DEVICE_UNKNOWN);
    WdfDeviceInitSetCharacteristics(deviceInit, FILE_DEVICE_SECURE_OPEN, FALSE);

    status = WdfDeviceInitAssignName(deviceInit, &deviceName);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDFDEVICE controlDevice = nullptr;
    status = WdfDeviceCreate(&deviceInit, WDF_NO_OBJECT_ATTRIBUTES, &controlDevice);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    // WdfDeviceCreate consumed deviceInit on success; don't free it.
    deviceInit = nullptr;

    // The DOS-name symlink is what CreateFile(L"\\\\.\\CursioAudio") resolves.
    status = WdfDeviceCreateSymbolicLink(controlDevice, &symlinkName);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    // A single default queue handles all IOCTLs sequentially — audio bridging
    // is ordered per endpoint, and PULL_RENDER is an inverted call the driver
    // pends, so sequential dispatch keeps ordering simple. (A dedicated
    // manual queue for pended PULLs comes with the RT-packet wiring.)
    WDF_IO_QUEUE_CONFIG queueCfg;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueCfg, WdfIoQueueDispatchSequential);
    queueCfg.EvtIoDeviceControl = Cursio_EvtIoDeviceControl;

    WDFQUEUE queue = nullptr;
    status = WdfIoQueueCreate(controlDevice, &queueCfg, WDF_NO_OBJECT_ATTRIBUTES, &queue);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    // Control devices must announce they're done initializing before they can
    // be opened.
    WdfControlFinishInitializing(controlDevice);

    g_CursioControlDevice = controlDevice;
    return STATUS_SUCCESS;
}
