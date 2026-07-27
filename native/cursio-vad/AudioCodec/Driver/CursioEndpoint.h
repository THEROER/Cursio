/*++

Cursio Virtual Audio Driver — dynamic endpoint devices.

NEW Cursio code (not from the Microsoft sample). Creates and destroys one child
audio device (a render+capture pair) per peer audio endpoint at runtime, so each
Linux-side device/app appears as its own native Windows endpoint. Driven by the
bridge's IOCTL_CURSIO_ADD_ENDPOINT / REMOVE_ENDPOINT.

The parent bus device is the sample's Codec device (ROOT\AudioCodec); child PDOs
are created under it via WdfPdoInitAllocate, following the ACX dynamic-device
enumeration sequence.

Environment: Kernel mode

--*/

#ifndef CURSIO_ENDPOINT_H
#define CURSIO_ENDPOINT_H

#include "CursioIoctl.h"   // CURSIO_ENDPOINT_ID, CURSIO_ENDPOINT_DESC
#include "CursioRing.h"    // CURSIO_RING

#ifdef __cplusplus
extern "C" {
#endif

//
// Borrow an endpoint's rings by id. Returns NULL if no such endpoint exists.
//
// Lifetime: the rings live in the endpoint registry and stay valid until
// Cursio_RemoveEndpoint tears the endpoint down. Callers on the streaming path
// re-resolve per packet rather than caching the pointer, so a removal between
// packets is seen as "endpoint gone" instead of a dangling read.
//
CURSIO_RING*
Cursio_GetRenderRing(
    _In_ CURSIO_ENDPOINT_ID Id
);

CURSIO_RING*
Cursio_GetCaptureRing(
    _In_ CURSIO_ENDPOINT_ID Id
);

//
// Remember the parent bus device (the Codec device) so dynamic endpoints can be
// created under it. Called once from Codec_EvtBusDeviceAdd after the device is
// created. Without a parent, AddEndpoint has nothing to hang child PDOs off.
//
VOID
Cursio_SetParentDevice(
    _In_ WDFDEVICE Parent
);

//
// Create a dynamic endpoint device (render+capture) for `Desc`. Returns
// STATUS_SUCCESS once the child device is registered with ACX and appearing.
// Idempotent-safe on failure: partial state is cleaned up.
//
NTSTATUS
Cursio_CreateEndpoint(
    _In_ const CURSIO_ENDPOINT_DESC* Desc
);

//
// Tear down a previously created endpoint by id.
//
NTSTATUS
Cursio_RemoveEndpoint(
    _In_ CURSIO_ENDPOINT_ID Id
);

#ifdef __cplusplus
}
#endif

#endif // CURSIO_ENDPOINT_H
