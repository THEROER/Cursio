# Cursio Virtual Audio Driver — fork notice

This directory is a **fork** of the **ACX AudioCodec sample** from Microsoft's
[Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples)
(`audio/Acx/Samples`), used as the base for Cursio's Windows virtual audio
driver.

## License

The original code is licensed under the **Microsoft Public License (MS-PL)** —
see [`LICENSE`](./LICENSE). Per MS-PL, this fork:

- **retains** all original Microsoft copyright/attribution notices in each
  source file (they are not removed — Cursio modification notes are added
  alongside them),
- **includes** the full MS-PL text (this repo ships `LICENSE`),
- does **not** use Microsoft names/logos as trademarks.

MS-PL permits forking, modification, and commercial/proprietary use, so this is
compatible with Cursio.

## What Cursio changes

The stock sample exposes one fixed virtual speaker + microphone and generates a
tone / saves to a file. Cursio turns it into a virtual audio bridge. All three
changes are implemented; Cursio's own code lives in files prefixed `Cursio*`, so
the sample's sources stay recognisable against upstream:

1. **Rename/identity** — unique GUIDs, driver tag, and device names for Cursio.
2. **User-mode data path** — the tone generator / file writer is replaced by a
   ring buffer fed over an IOCTL channel (`CursioRing`, `CursioBridge`,
   `Shared/CursioIoctl.h`), so Cursio's user-mode process pushes decoded PCM in
   (capture endpoints) and pulls rendered PCM out (render endpoints). The
   streaming path is hooked through `Shared/CursioStreamHook.h` in
   `StreamEngine`, `RenderCircuit` and `CaptureCircuit`. The user-mode half is
   `crates/audio/src/vad_client.rs`. See `docs/audio-design.md`.
3. **Dynamic devices** — instead of one fixed speaker+mic created at
   `PrepareHardware`, `Cursio_CreateEndpoint` / `Cursio_RemoveEndpoint`
   (`CursioEndpoint.cpp`) create one render+capture child device *per remote
   device/app* on demand via `AcxDeviceAddCircuitDevice`, so the peer's devices
   appear as native Windows endpoints (parity with the Linux/PipeWire side).

## Status

Written, **not yet in service.** No built and signed `.sys` ships, so the driver
has not been loaded and exercised on a real Windows machine. Until it is, a peer's
devices do not appear in Windows' own device list, and `--audio` on Windows works
through WASAPI capture/render only.
