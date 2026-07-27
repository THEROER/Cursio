/*++

Cursio Virtual Audio Driver — per-endpoint PCM ring buffers.

NEW Cursio code (not from the Microsoft sample). See CursioRing.h for the
design, the lossy overrun/underrun policy, and the locking rationale.

Environment: Kernel mode

--*/

#include "public.h"     // WDF/ACX/initguid (include first, once)
#include "CursioRing.h"

//
// Pool tag, shown as "CurR" in pool dumps ('CurR' reversed for little-endian).
//
#define CURSIO_RING_POOL_TAG 'RruC'

_Use_decl_annotations_
NTSTATUS
Cursio_RingInit(
    CURSIO_RING* Ring
)
{
    RtlZeroMemory(Ring, sizeof(*Ring));

    // Non-paged: the stream side touches this at DISPATCH_LEVEL, where a page
    // fault would bugcheck.
    Ring->Buffer = (PUCHAR)ExAllocatePool2(
        POOL_FLAG_NON_PAGED, CURSIO_RING_BYTES, CURSIO_RING_POOL_TAG);
    if (Ring->Buffer == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    KeInitializeSpinLock(&Ring->Lock);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
VOID
Cursio_RingFree(
    CURSIO_RING* Ring
)
{
    if (Ring->Buffer != nullptr) {
        ExFreePoolWithTag(Ring->Buffer, CURSIO_RING_POOL_TAG);
        Ring->Buffer = nullptr;
    }
    Ring->Head = Ring->Tail = Ring->Used = 0;
}

//
// Copy into the ring at Head, wrapping once. Caller holds the lock and has
// already ensured there is room.
//
static VOID
Cursio_RingCopyIn(
    _Inout_ CURSIO_RING* Ring,
    _In_reads_bytes_(Bytes) const UCHAR* Data,
    _In_ ULONG Bytes
)
{
    ULONG firstChunk = CURSIO_RING_BYTES - Ring->Head;
    if (firstChunk > Bytes) {
        firstChunk = Bytes;
    }

    RtlCopyMemory(Ring->Buffer + Ring->Head, Data, firstChunk);
    if (Bytes > firstChunk) {
        RtlCopyMemory(Ring->Buffer, Data + firstChunk, Bytes - firstChunk);
    }

    Ring->Head = (Ring->Head + Bytes) % CURSIO_RING_BYTES;
    Ring->Used += Bytes;
}

_Use_decl_annotations_
VOID
Cursio_RingWrite(
    CURSIO_RING* Ring,
    const UCHAR* Data,
    ULONG Bytes
)
{
    if (Ring->Buffer == nullptr || Bytes == 0) {
        return;
    }

    // A write bigger than the whole ring can only keep its tail; drop the rest
    // up front so the wrap logic below stays single-pass.
    if (Bytes > CURSIO_RING_BYTES) {
        Data += (Bytes - CURSIO_RING_BYTES);
        Bytes = CURSIO_RING_BYTES;
    }

    KLOCK_QUEUE_HANDLE lockHandle;
    KeAcquireInStackQueuedSpinLock(&Ring->Lock, &lockHandle);

    // Overrun: discard the oldest bytes to make room. Newest audio wins — a
    // stalled consumer must not push us into playing stale sound.
    ULONG freeBytes = CURSIO_RING_BYTES - Ring->Used;
    if (Bytes > freeBytes) {
        ULONG toDrop = Bytes - freeBytes;
        Ring->Tail = (Ring->Tail + toDrop) % CURSIO_RING_BYTES;
        Ring->Used -= toDrop;
        Ring->Dropped += toDrop;
    }

    Cursio_RingCopyIn(Ring, Data, Bytes);

    KeReleaseInStackQueuedSpinLock(&lockHandle);
}

_Use_decl_annotations_
ULONG
Cursio_RingRead(
    CURSIO_RING* Ring,
    UCHAR* Data,
    ULONG Bytes
)
{
    if (Bytes == 0) {
        return 0;
    }
    if (Ring->Buffer == nullptr) {
        RtlZeroMemory(Data, Bytes);
        return 0;
    }

    KLOCK_QUEUE_HANDLE lockHandle;
    KeAcquireInStackQueuedSpinLock(&Ring->Lock, &lockHandle);

    ULONG available = (Ring->Used < Bytes) ? Ring->Used : Bytes;

    if (available > 0) {
        ULONG firstChunk = CURSIO_RING_BYTES - Ring->Tail;
        if (firstChunk > available) {
            firstChunk = available;
        }

        RtlCopyMemory(Data, Ring->Buffer + Ring->Tail, firstChunk);
        if (available > firstChunk) {
            RtlCopyMemory(Data + firstChunk, Ring->Buffer, available - firstChunk);
        }

        Ring->Tail = (Ring->Tail + available) % CURSIO_RING_BYTES;
        Ring->Used -= available;
    }

    // Underrun: pad with silence so the caller always hands the audio engine a
    // fully-populated packet rather than leaking whatever was in the buffer.
    if (available < Bytes) {
        RtlZeroMemory(Data + available, Bytes - available);
        Ring->Starved += (Bytes - available);
    }

    KeReleaseInStackQueuedSpinLock(&lockHandle);

    return available;
}

_Use_decl_annotations_
ULONG
Cursio_RingUsed(
    CURSIO_RING* Ring
)
{
    if (Ring->Buffer == nullptr) {
        return 0;
    }

    KLOCK_QUEUE_HANDLE lockHandle;
    KeAcquireInStackQueuedSpinLock(&Ring->Lock, &lockHandle);
    ULONG used = Ring->Used;
    KeReleaseInStackQueuedSpinLock(&lockHandle);

    return used;
}
