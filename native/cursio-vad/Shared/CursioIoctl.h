/*++

Cursio Virtual Audio Driver — user-mode <-> kernel IOCTL contract.

This header is NEW Cursio code (not from the Microsoft sample) and is shared
verbatim between the kernel driver and Cursio's user-mode process, so both sides
agree on the control-device name, the IOCTL codes, and the wire structs. Keep it
free of kernel- or user-only headers.

Design (see the audio design docs / the VAD architecture memo):

  * The per-stream AUDIO samples themselves ride ACX RT packets (shared memory
    the OS maps into user mode) — NOT these IOCTLs. These IOCTLs are the
    *bridge* that moves that PCM between the kernel driver and Cursio's user-mode
    process, which then carries it over the network to/from the Linux peer.

  * RENDER stream  (a Windows OUTPUT device = a peer speaker): Windows apps play
    into it; the driver hands the rendered PCM up to Cursio via
    IOCTL_CURSIO_PULL_RENDER (an inverted call the user-mode side keeps pending;
    the driver completes it when a packet is ready).

  * CAPTURE stream (a Windows INPUT device = a peer source): Cursio pushes PCM
    that arrived from the peer down via IOCTL_CURSIO_PUSH_CAPTURE; the driver
    feeds it into the capture RT packets.

  * Devices are created/destroyed on demand (one render+capture pair per peer
    audio endpoint) via IOCTL_CURSIO_ADD_ENDPOINT / IOCTL_CURSIO_REMOVE_ENDPOINT.

--*/

#ifndef CURSIO_IOCTL_H
#define CURSIO_IOCTL_H

//
// Control device the user-mode process opens. The driver creates a symbolic
// link with this name in \DosDevices, so CreateFile(L"\\\\.\\CursioAudio", ...)
// from user mode reaches us.
//
#define CURSIO_CONTROL_DEVICE_NAME    L"\\Device\\CursioAudio"
#define CURSIO_CONTROL_SYMLINK_NAME   L"\\DosDevices\\CursioAudio"
#define CURSIO_CONTROL_DOS_NAME       L"\\\\.\\CursioAudio"

//
// Protocol version, bumped when any struct or IOCTL below changes so a
// mismatched user-mode build is rejected at IOCTL_CURSIO_HELLO rather than
// corrupting audio.
//
#define CURSIO_PROTOCOL_VERSION       1u

//
// Our device type for the IOCTL codes. 0x8000+ is the vendor-reserved range.
//
#define CURSIO_DEVICE_TYPE            0x8039

//
// IOCTL codes. METHOD_BUFFERED keeps the marshalling simple (the framework
// copies to/from a system buffer); FILE_ANY_ACCESS because access is gated by
// the control device's ACL, not per-IOCTL. Function codes start at 0x800
// (0x000-0x7FF is Microsoft-reserved).
//
#define CTL_CODE_CURSIO(fn) \
    CTL_CODE(CURSIO_DEVICE_TYPE, (fn), METHOD_BUFFERED, FILE_ANY_ACCESS)

// Handshake: user mode sends its CURSIO_PROTOCOL_VERSION, driver replies with
// its own. Must be the first call; a version mismatch fails the session.
#define IOCTL_CURSIO_HELLO            CTL_CODE_CURSIO(0x800)

// Create a peer audio endpoint (a render+capture pair). Input: CURSIO_ENDPOINT_DESC.
#define IOCTL_CURSIO_ADD_ENDPOINT     CTL_CODE_CURSIO(0x801)

// Destroy a previously added endpoint. Input: CURSIO_ENDPOINT_ID.
#define IOCTL_CURSIO_REMOVE_ENDPOINT  CTL_CODE_CURSIO(0x802)

// Push PCM from the peer into a CAPTURE stream. Input: CURSIO_PCM_CHUNK header
// immediately followed by `Bytes` bytes of interleaved PCM.
#define IOCTL_CURSIO_PUSH_CAPTURE     CTL_CODE_CURSIO(0x803)

// Pull rendered PCM out of a RENDER stream (inverted/pended call). Output: a
// CURSIO_PCM_CHUNK header followed by up to the output buffer's worth of PCM.
// The driver completes it when a packet is ready, or on endpoint teardown.
#define IOCTL_CURSIO_PULL_RENDER      CTL_CODE_CURSIO(0x804)

//
// A stable, session-scoped endpoint id assigned by the user-mode side (mirrors
// Cursio's AudioStreamId). One id names a render+capture pair.
//
typedef unsigned int CURSIO_ENDPOINT_ID;

//
// PCM format shared across the bridge. Kept fixed and explicit rather than
// negotiated per stream for the first cut: 48 kHz / stereo / 16-bit matches the
// wire default and the ACX sample's format. Extend when variable formats land.
//
typedef struct _CURSIO_PCM_FORMAT {
    unsigned int  SampleRate;   // e.g. 48000
    unsigned short Channels;    // e.g. 2
    unsigned short BitsPerSample; // e.g. 16
} CURSIO_PCM_FORMAT;

//
// Endpoint description for IOCTL_CURSIO_ADD_ENDPOINT. `Name` is a UTF-16
// friendly name shown in the Windows sound control panel (e.g.
// "Cursio: Spotify (Laptop)"); it is a fixed-size field to keep the struct
// trivially copyable across the buffered IOCTL.
//
#define CURSIO_MAX_NAME_CHARS 128

typedef struct _CURSIO_ENDPOINT_DESC {
    CURSIO_ENDPOINT_ID  Id;
    CURSIO_PCM_FORMAT   Format;
    wchar_t             Name[CURSIO_MAX_NAME_CHARS]; // NUL-terminated
} CURSIO_ENDPOINT_DESC;

//
// Just an id, for IOCTL_CURSIO_REMOVE_ENDPOINT.
//
typedef struct _CURSIO_ENDPOINT_ID_ARG {
    CURSIO_ENDPOINT_ID  Id;
} CURSIO_ENDPOINT_ID_ARG;

//
// Header prefixing PCM in PUSH_CAPTURE (input) and PULL_RENDER (output). The
// samples follow immediately in the same buffer, `Bytes` long. `Seq` lets the
// far side order/detect loss the same way the network AudioChunk does.
//
typedef struct _CURSIO_PCM_CHUNK {
    CURSIO_ENDPOINT_ID  Id;
    unsigned int        Seq;
    unsigned int        Bytes;   // number of PCM bytes following this header
    unsigned int        Reserved;
} CURSIO_PCM_CHUNK;

//
// Reply to IOCTL_CURSIO_HELLO.
//
typedef struct _CURSIO_HELLO_REPLY {
    unsigned int  DriverProtocolVersion;
} CURSIO_HELLO_REPLY;

#endif // CURSIO_IOCTL_H
