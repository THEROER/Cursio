/*++

Cursio Virtual Audio Driver — per-endpoint PCM ring buffers.

NEW Cursio code (not from the Microsoft sample).

Each endpoint owns two rings, one per direction:

  * RenderRing  — driver -> user mode. The Windows audio engine renders PCM into
    the render stream's RT packets; ProcessPacket writes those bytes here, and
    IOCTL_CURSIO_PULL_RENDER drains them up to Cursio, which sends them to the
    peer.

  * CaptureRing — user mode -> driver. IOCTL_CURSIO_PUSH_CAPTURE writes PCM that
    arrived from the peer here, and the capture stream's ProcessPacket fills its
    RT packets from it, so Windows apps see the peer as a recording source.

Concurrency: the producer and consumer of a given ring are always different
threads (a stream timer at DISPATCH_LEVEL on one side, an IOCTL at
PASSIVE_LEVEL on the other), so each ring carries its own spin lock. A spin lock
— not a fast mutex — because the stream side runs at DISPATCH_LEVEL, where
mutexes cannot be taken. Buffers are non-paged for the same reason.

Overrun/underrun policy is deliberately lossy rather than blocking: audio is a
real-time stream, so when a ring is full the OLDEST bytes are dropped (the
newest audio matters), and when it is empty the consumer reads SILENCE. Never
stall a stream callback waiting on the network.

Environment: Kernel mode

--*/

#ifndef CURSIO_RING_H
#define CURSIO_RING_H

#ifdef __cplusplus
extern "C" {
#endif

//
// Ring capacity. At the bridge's fixed format (48 kHz stereo 16-bit =
// 192000 bytes/sec) 64 KB is ~170 ms of audio: enough to absorb network jitter
// and scheduling hiccups without adding gross latency. Anything older than this
// is not worth playing anyway.
//
#define CURSIO_RING_BYTES (64u * 1024u)

typedef struct _CURSIO_RING {
    KSPIN_LOCK  Lock;
    ULONG       Head;       // write offset
    ULONG       Tail;       // read offset
    ULONG       Used;       // bytes currently stored (0..CURSIO_RING_BYTES)
    ULONGLONG   Dropped;    // bytes discarded on overrun (diagnostics)
    ULONGLONG   Starved;    // bytes filled with silence on underrun
    PUCHAR      Buffer;     // non-paged, CURSIO_RING_BYTES long
} CURSIO_RING;

//
// Allocate the ring's backing buffer and reset state. Returns
// STATUS_INSUFFICIENT_RESOURCES if the non-paged allocation fails.
//
NTSTATUS
Cursio_RingInit(
    _Out_ CURSIO_RING* Ring
);

//
// Free the backing buffer. Safe to call on a zeroed/never-initialized ring.
//
VOID
Cursio_RingFree(
    _Inout_ CURSIO_RING* Ring
);

//
// Write `Bytes` from `Data`. On overrun the oldest buffered bytes are dropped to
// make room, so this always accepts the whole write (a write larger than the
// ring keeps only its trailing CURSIO_RING_BYTES). Callable at DISPATCH_LEVEL.
//
VOID
Cursio_RingWrite(
    _Inout_ CURSIO_RING* Ring,
    _In_reads_bytes_(Bytes) const UCHAR* Data,
    _In_ ULONG Bytes
);

//
// Read up to `Bytes` into `Data`. Any shortfall is zero-filled (silence) so the
// caller always gets a full, playable buffer. Returns the number of REAL bytes
// read (i.e. excluding the silence padding), which lets the caller tell an idle
// stream from a starved one. Callable at DISPATCH_LEVEL.
//
ULONG
Cursio_RingRead(
    _Inout_ CURSIO_RING* Ring,
    _Out_writes_bytes_(Bytes) UCHAR* Data,
    _In_ ULONG Bytes
);

//
// Bytes currently available to read. Informational only — it can change the
// moment the lock is dropped.
//
ULONG
Cursio_RingUsed(
    _Inout_ CURSIO_RING* Ring
);

#ifdef __cplusplus
}
#endif

#endif // CURSIO_RING_H
