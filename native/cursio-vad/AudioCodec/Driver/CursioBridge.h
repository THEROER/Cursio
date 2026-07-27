/*++

Cursio Virtual Audio Driver — user-mode bridge (control device + IOCTL).

NEW Cursio code (not from the Microsoft sample). Creates a WDFDEVICE control
device that Cursio's user-mode process opens to move PCM between the network and
the ACX render/capture streams, and to create/destroy peer endpoints on demand.
Layered on top of the sample so the sample's ACX scaffolding stays untouched.

Environment: Kernel mode

--*/

#ifndef CURSIO_BRIDGE_H
#define CURSIO_BRIDGE_H

// Intentionally does NOT include "public.h": that header pulls in <initguid.h>
// and has no include guard, so including it from here (and again from the .cpp)
// double-defines its GUIDs and WDF contexts. Callers include "public.h"
// themselves before this header; we only forward-declare what our one prototype
// needs. <wdf.h> (via the includer) provides WDFDRIVER and NTSTATUS.

#ifdef __cplusplus
extern "C" {
#endif

//
// Create the Cursio control device and its IOCTL queue. Called once from
// DriverEntry, after AcxDriverInitialize. On failure it cleans up and returns
// the status; the driver load fails if the bridge can't come up.
//
NTSTATUS
Cursio_CreateControlDevice(
    _In_ WDFDRIVER Driver
);

#ifdef __cplusplus
}
#endif

#endif // CURSIO_BRIDGE_H
