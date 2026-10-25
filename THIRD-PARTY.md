# Third-party code

Cursio is licensed under the GNU General Public License, version 3 or (at your
option) any later version — see [`LICENSE`](LICENSE). This file names the code
in this repository that Cursio did not write, and the licenses that decided
Cursio's own.

## Why GPL-3.0

Two components the desktop builds link are GPL-3.0, so every binary that
contains them is a GPL-3.0 work whatever the rest of the source says:

- **playfair** — the FairPlay setup an iPhone requires before it will mirror to
  a receiver. It is compiled into `cursio-airplay`, which `cursio-core` links
  by default (`airplay` is one of the `desktop` features).
- **winfsp / winfsp-sys** — the Rust bindings through which a Windows node
  mounts a peer's disks. WinFsp itself is GPL-3.0 with a FLOSS exception; the
  bindings are plain GPL-3.0.

Licensing the libraries permissively while every shipped binary is GPL would
describe a choice nobody can actually make, so the whole project is GPL.

## Vendored source

| Path | Origin | License |
|---|---|---|
| `crates/airplay/native/playfair/playfair/` | playfair, a FairPlay SAP implementation | GPL-3.0 — [`LICENSE.md`](crates/airplay/native/playfair/playfair/LICENSE.md) beside it |
| `crates/airplay/native/playfair/fairplay.h`, `fairplay_playfair.c` | shairplay, © 2018 Juho Vähä-Herttua | LGPL-2.1-or-later, per the file headers |
| `native/cursio-vad/` | Microsoft's ACX AudioCodec sample from [Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples) | MS-PL — [`LICENSE`](native/cursio-vad/LICENSE) beside it; changes listed in [`CURSIO-FORK.md`](native/cursio-vad/CURSIO-FORK.md) |

The Windows audio driver stays under MS-PL. MS-PL and the GPL cannot be
combined into one program, and the driver is not one program with Cursio: it is
a separate kernel-mode binary that `crates/audio/src/vad_client.rs` talks to
over IOCTL, and nothing of it is compiled into a Cursio executable. Keep it that
way — moving driver code into a crate, or GPL code into the driver, would make
the result undistributable.

## Dependencies

Everything else arrives through Cargo and bun. Apart from the two GPL-3.0
crates above, the Rust dependency graph is permissive (MIT, Apache-2.0, BSD,
ISC, Zlib, Unicode-3.0, BSL-1.0) or weak copyleft (MPL-2.0, in the CSS parser
Tauri's build-time code generation uses), all of which GPL-3.0 can include. To
list it:

```sh
cargo metadata --format-version 1 | jq -r '.packages[] | select(.source) | "\(.name) \(.version) \(.license)"'
```

libopus (BSD-3-Clause) is compiled in statically by its crates. The system
libraries are linked dynamically and not shipped in this repository: FFmpeg
(`libav*`, LGPL or GPL depending on how the distribution built it), GStreamer
(LGPL), PipeWire (MIT), GTK4 and libadwaita (LGPL). A Windows bundle that
carries GStreamer's DLLs carries their LGPL obligations with them.
