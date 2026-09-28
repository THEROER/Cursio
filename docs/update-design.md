# Updating Cursio

How a machine in the mesh gets a new build, switches to it, and gets back off
it when it is bad. Phase 1 (the local half) and phase 2 (delivery over the
mesh) are built; phases 3–4 are the design they will follow and are **not
built yet**.

## What was wrong with "copy the new exe over"

Until now an update was: build on Nocturne, `scp` a `.exe` to Lightmoon under a
fresh name (a running executable is locked on Windows, so the old name could
not be reused), and restart through an SSH session into WSL and a scheduled
task. It worked for one developer with SSH into both machines, and it left
seventy-five `cursio-vNN.exe` files and a recurring question — *which build is
actually running?* — that cost more time than any bug it hid.

Two properties were missing: a build should have exactly one place it lives,
and switching to it should be one reversible act.

## Borrowed from the LeavePulse launcher

`launcher-updater-helper` (same author, Apache-2.0) already solves the
mechanical half: the launcher verifies a bundle, hands off to a separate helper
process and exits; the helper waits for the launcher's pid, stops the daemon,
swaps the executables, and relaunches. A separate process because a running
image cannot replace itself; run from a copy in a temporary directory so that
the helper is not itself one of the files being replaced.

Cursio's `update::apply` is ported from it — the wait-for-exit on both
platforms, "verify every component before touching anything", the relocation
to a temporary copy, the zstd `--patch-from` delta mode. What changes is the
swap itself, below.

## Phase 1: the layout (built)

```text
<root>/versions/0.1.0+c9798e7060/{cursio, cursio-app, manifest.json}
<root>/versions/0.1.0+d41c2e0a11/...
<root>/current     "0.1.0+d41c2e0a11"
<root>/previous    "0.1.0+c9798e7060"
<root>/cursio, <root>/cursio-app          stable copies of current's
```

| Platform | Root | Why there |
|---|---|---|
| Windows | `%LOCALAPPDATA%\Cursio` | per user, not roamed (a build for this machine must not follow the user elsewhere), writable without elevation |
| Linux | `~/.local/lib/cursio` | executables are not data (`~/.local/share`); `lib` is where per-user installs of whole programs go |
| macOS | `~/Library/Application Support/Cursio` | the one per-user place an unsigned build may write itself |

`CURSIO_INSTALL_ROOT` overrides all three (tests; a second node on one machine).

**Side by side instead of in place.** The launcher moves the installed files
aside and restores them if the swap fails. Here the new build is written to
its own directory while the old one keeps running, so nothing that is running
is ever touched, and switching is rewriting one pointer. Going back is
rewriting it again — the previous build is intact until a later prune.

**Pointers are files holding a name, not symlinks.** A symlink needs a
privilege on Windows, and "points at a missing directory" is answered
differently per platform. Writing a temporary file and renaming it over the
pointer is atomic everywhere Cursio runs: a crash mid-switch leaves the old
name or the new one. A pointer naming a directory that does not exist, or a
name that is not a plain version (`..`, separators), reads as absent.

**Stable copies are the only paths anything outside registers.** Autostart, the
app's "start node", shortcuts and the Windows `Run` key name `<root>/cursio`,
which never changes. Started from there, the executable hands over to the
current version's copy at once and exits (`update::layout::handover`), marking
the child so it can never loop. A registry entry naming
`versions/<v>/cursio.exe` would outlive that version by one prune. Development
builds under `target/` are never under the root and never hand over.

**Installing** (`cursio update install <dir>`) reads the build's
`manifest.json`, verifies every component's sha256 in the source, copies into
`versions/.<v>.partial`, verifies again, and renames into place — a half-copied
build never looks installed. `--delta-from <v>` builds the new version by
patching an installed one with zstd deltas instead, checking the patch before
and the result after, exactly as the launcher does. `cargo xtask stage
[host|windows] [--install]` produces such a directory, named
`<crate version>+<commit>` and, from a dirty tree, `.dirty.<hash>` too, so two
different uncommitted builds never share a name.

**Switching** (`cursio update apply --version V --wait-pid <node> [--stop]`):

1. If running from inside the install, copy itself to a temporary directory,
   start the copy with the same arguments, and exit.
2. Check that `V` is installed and still intact. Nothing has been stopped yet.
3. Wait for each `--wait-pid` to exit (asking it to first with `--stop`), up to
   30 s; a process that will not exit abandons the update with nothing changed.
4. Point `current` at `V`, `previous` at what it replaces, refresh the stable
   copies.
5. Start the node from `V`, detached, with the given arguments (`mesh` by
   default — settings live in `config.toml`, not flags).
6. Wait for a `Ping` answered on this user's control socket. After the old
   processes are gone, whatever answers is the node just started. No answer in
   time (90 s by default — an unelevated start raises a UAC prompt and a person
   has to click it) rolls `current` back and starts the previous version.
7. Prune to the newest three, never `current`, `previous` or `V`.

`cursio update rollback` is the same walk towards `previous`; `status` lists
what is installed; `prune` deletes old versions.

**Elevation is inherited, not asked for again.** The helper runs with the
token of whoever started it. Started by an elevated node, it is elevated, and so
is the node it starts — no UAC prompt for an update nobody sat down for. Started
without a console, it starts the node without one (`DETACHED_PROCESS`), so no
window flashes up either.

**Stopping in phase 1 is blunt on Windows.** `--stop` sends SIGINT on Unix,
which the node shuts down cleanly on. Windows has no signal for a process
without a console, so it terminates the process. A node switching itself
(phase 2) first waits for a quiet moment, which is what makes that tolerable;
a stop request over the control socket, letting it tell its peers first, is
not built.

## Phase 2: delivery over the mesh (built)

The mesh is already a trusted, encrypted channel between exactly the machines
that need the build, so an update rides it rather than SSH, and needs no
internet. Code: `protocol/src/update.rs` (the wire), `core/src/update/`
(`sign`, `store`, `lane`, `agent`, `quiet`).

**The exchange survives version skew.** A `PROTOCOL_VERSION` mismatch used to
close the link, which meant precisely the machine that needed an update could
not hear about it. Now every connection opens with a `Preamble` in the update
lane's own encoding: magic `CUPD`, a lane byte that is always `1`, a kind byte,
then fields laid out by hand — integers big-endian, strings as a `u32` length
and bytes. Tests pin those bytes, and `Message::Update` keeps variant index 17
while `Message::Hello` keeps 0. Both sides learn each other's version from the
preamble; equal versions go on to `Hello`, different ones skip it, encrypt with
the pair's stored key, and carry the lane alone (`lane::run_alone`) until
neither side has anything left to say. The lane grows by new kinds, which old
builds skip; an existing kind never changes. The one gap is history: a build
from before v19 opens with its old `Hello`, is recognised by it, and has to be
updated by hand once.

**Offer, then pull, a window at a time.** A node offers every build it holds
(`store/<version>/offer` plus the platforms it has in full) when a link comes
up and again every 30 s. A receiver that wants one asks for 1 MiB of a file at
a time and gets it in 256 KiB chunks at `Bulk` priority, so a build queues
behind the cursor like any file chunk and a sender is never more than one
window ahead. One build is fetched at a time per machine however many peers
offer it. Received files go to `store/`, are checked against the signed size
and sha256, and then installed into `versions/` through phase 1's `install`.
The store keeps them, so the receiver can serve the next machine; a machine
fetches only its own platform, and relays what it holds.

**A platform is where a build runs, not just an OS.** On Windows it is
`windows-x86_64`. On Linux the node links the system's FFmpeg by soname, and
that soname follows FFmpeg's major version: a build from Ubuntu 25.10
(`libavutil.so.59`) does not start on 24.04 (`.so.58`). Named plain
`linux-x86_64`, the 24.04 machine took the newer build, failed the health check
and rolled back, and did so again at every offer. So a Linux platform carries
the FFmpeg major it links — `linux-x86_64-av59`, `linux-x86_64-av58` — read by
`crates/core/build.rs` from the same `pkg-config` the FFmpeg bindings use, and
by `xtask` the same way when it names what it staged. `cargo xtask publish` on a
Linux host builds the 24.04 variant in a container
(`crates/xtask/docker/ubuntu-24.04.Dockerfile`, `cargo xtask stage ubuntu24`)
and signs it into the same version, so one publish reaches both. A build
without the window module links no FFmpeg and keeps the plain name. Nodes from
before the split call themselves `linux-x86_64` and are updated by hand once.

**Signatures, not trust in whoever sent it.** What is signed is a `Statement`:
version, channel, publish time (builds are ordered by it — commits do not
sort), and every platform's files with size and sha256. Ed25519, two kinds of
key:

- **release keys**, compiled into `sign::RELEASE_KEYS` — they sign `stable`.
  There are none until phase 3, and a stable channel with no key accepts
  nothing rather than a placeholder;
- **builder keys**: each publishing machine makes its own
  (`update-signing.key` beside its identity). A receiver accepts `dev` builds
  from it only after `cursio update trust-builder <machine>`, which pins the
  key it signs with and switches that receiver to `dev`. Being paired is not
  enough — pairing grants input and files, not the right to replace the
  program. The key of the last build a machine offered is remembered, so
  trusting it needs no key copied by hand (`--key` gives it explicitly).

A refused build is a `diagnostics` problem naming the reason, and is asked
again each time it is offered, so trusting the builder afterwards takes effect
without a restart.

**Installing is not switching.** Under `[update] policy`:

- `ask` (the default) installs, raises "ready to switch", and waits for the
  app's button or `update_apply` on the control socket;
- `auto` installs, then switches at the first quiet moment, checked every 15 s;
- `never` fetches nothing (and still relays what it holds).

**Quiet** (`quiet::busy`) means: this machine is not driving a peer's screen,
and no file-sharing or USB message has crossed any link for 60 s — a restart
mid-write is how Steam wedged itself on a WinFsp mount. Switching hands over to
phase 1's helper with the node's own arguments; the helper stops the node
(SIGINT on Unix, which shuts down cleanly; termination on Windows), flips
`current`, starts the new build with the old one's elevation, and rolls back
if it does not answer.

Not done from the original plan: an AirPlay session does not yet hold the
switch, and the node does not tell its peers it is leaving before the helper
stops it — they notice at the heartbeat timeout.

**What it replaces.** `cargo xtask publish` stages this machine's build and the
Windows one as one version, and the staged node signs them into its own store
(`cursio update publish`), from which the running node offers them. Lightmoon
updates itself. `cargo xtask deploy` stays until this has run on both machines.

## Phase 3: public releases (not built)

GitHub Releases carry the same signed manifest and a bundle per platform. A
node with internet checks the release feed for the `stable` channel and fetches
from there; it then offers what it fetched to its peers, so one download
serves the mesh. Installers as the launcher ships them: a signed NSIS installer
on Windows (Authenticode, same Azure signing profile), `.deb` and AppImage on
Linux. The installer only lays out `<root>` and the stable copies; everything
after is the updater.

## Phase 4: Android and macOS (not built)

**Android** cannot replace its own APK silently. An update is handed to the
system `PackageInstaller`, which asks the user; or it comes from a store. The
mesh can still deliver the APK — it is a file — but the last step is the
user's. The node lives in the app process, so there is no helper and no
pointer: the installer replaces the app and Android restarts it.

**macOS** needs a signed and notarized `.app`. Replacing a bundle in
`~/Applications` is allowed; replacing one that lost its signature trips
Gatekeeper and resets the Accessibility and Screen Recording grants, which are
tied to the signing identity. Until builds are signed with a stable identity,
every update costs the user those two permission prompts again, so macOS stays
on manual updates until then.
