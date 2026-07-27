/*++

Cursio Virtual Audio Driver — the seam between the sample's stream engines and
Cursio's per-endpoint rings.

NEW Cursio code (not from the Microsoft sample).

The sample's CStreamEngine lives in Common/ and knows nothing about Cursio. To
keep the fork shallow (see CURSIO-FORK.md) this header is the ONLY Cursio
surface Common/StreamEngine.cpp touches: two calls that move a packet's worth of
PCM, plus one lookup that resolves which endpoint a stream belongs to.

It lives in Shared/ (not next to its implementation in AudioCodec/Driver/)
because Shared/ is the one include path BOTH the SamplesCommon and the Driver
projects carry. The implementation stays in CursioEndpoint.cpp, where the
endpoint registry and its rings are; Common/ only ever sees these prototypes and
links against them.

How a stream finds its endpoint: endpoints are child PDOs created by
Cursio_CreateEndpoint, one device per endpoint id, and the circuits hang off
that child device. So the WDFDEVICE handed to EvtCircuitCreateStream uniquely
identifies the endpoint — Cursio_LookupEndpointByDevice maps it back to an id,
which the engine caches for the life of the stream.

CURSIO_INVALID_ENDPOINT means "this stream is not a Cursio endpoint" (e.g. the
sample's own static circuits on the parent device). Engines carrying it fall
back to the sample's original behaviour, so an unmodified AudioCodec device
keeps working exactly as before.

Environment: Kernel mode

--*/

#ifndef CURSIO_STREAM_HOOK_H
#define CURSIO_STREAM_HOOK_H

#include "CursioIoctl.h"    // CURSIO_ENDPOINT_ID

#ifdef __cplusplus
extern "C" {
#endif

//
// Sentinel for "not a Cursio endpoint stream".
//
#define CURSIO_INVALID_ENDPOINT ((CURSIO_ENDPOINT_ID)~0u)

//
// Map a stream's WDFDEVICE back to the endpoint id it was created for.
// Returns CURSIO_INVALID_ENDPOINT if the device is not a Cursio endpoint.
//
CURSIO_ENDPOINT_ID
Cursio_LookupEndpointByDevice(
    _In_ WDFDEVICE Device
);

//
// RENDER path (Windows OUTPUT -> peer speaker): hand one RT packet's worth of
// rendered PCM to the endpoint's render ring, where PULL_RENDER picks it up.
// No-op for CURSIO_INVALID_ENDPOINT. Callable at DISPATCH_LEVEL.
//
VOID
Cursio_StreamPushRender(
    _In_ CURSIO_ENDPOINT_ID Id,
    _In_reads_bytes_(Bytes) const UCHAR* Data,
    _In_ ULONG Bytes
);

//
// CAPTURE path (peer source -> Windows INPUT): fill one RT packet from the
// endpoint's capture ring, zero-padding on underrun. Returns the number of real
// (non-silence) bytes. For CURSIO_INVALID_ENDPOINT the buffer is left untouched
// and 0 is returned, so the caller can run its original fallback instead.
// Callable at DISPATCH_LEVEL.
//
ULONG
Cursio_StreamPullCapture(
    _In_ CURSIO_ENDPOINT_ID Id,
    _Out_writes_bytes_(Bytes) UCHAR* Data,
    _In_ ULONG Bytes
);

#ifdef __cplusplus
}
#endif

#endif // CURSIO_STREAM_HOOK_H
