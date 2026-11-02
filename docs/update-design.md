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

On Linux the node also puts the app in the desktop's application list
whenever it runs from the root (`update::launcher`): an entry naming
`<root>/cursio-app` plus the icon, which is compiled in. A machine fed only by
the mesh has no checkout to run `cargo xtask install` from, so without this
the app was installed and up to date there and search found nothing. The node
writes it at its own start, not in the apply helper, because the helper is the
previous version's code.

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

Such a link is also a problem in the status (`health::version_skew`), and the
problem says which machine is behind and how it catches up. "Protocol 22 vs 23"
left a person to work out both: the older machine takes the newer one's build
once it trusts that machine as a builder, so the hint names the exact
`cursio update trust-builder` to run, and where. It clears when the two greet
on one protocol.

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

An offered build already in `versions/` — installed by hand with `cursio update
install`, or fetched by a run that stopped before switching — is not fetched
again but is ready all the same, so policy switches to it. Skipping it, as the
agent first did, left a machine on its older build for good. The exception is
a build named by `previous`: that is where a rollback leaves the one that did
not come up, and offering it again must not switch back to it.

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
no peer's input has been injected here for 60 s (a peer driving *this* screen
is the user working here, and a switch would throw the cursor back mid-word),
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

## Phase 4: Android (built) and macOS (not built)

**Android** cannot replace its own APK silently, so the mesh delivers it and
the person installs it. `cargo xtask publish` builds the APK too wherever an
SDK with an NDK and bun are present (`--no-android` leaves it out) and stages
it as the platform `android-aarch64`, whose one component is `apk`. The
phone's node runs the same agent and lane as a computer, with its store under
the app's data directory (`crates/android/src/node/update.rs`), and fetches,
verifies and installs the APK into `versions/` the same way. What differs is
the switch: the agent is given a `Switcher` that hands the APK to Kotlin
(`UpdateOffer`), which posts a notification; its tap opens the system
installer on the file, served by the plugin's own `UpdateFileProvider`. The
first time, Android also asks to let Cursio install apps. The installer
replaces the app and Android restarts it; there is no helper and no pointer.

While a build comes in, the phone shows it: the control status lists every
fetch with its bytes (`fetching_builds`, control v23, from each agent's
`progress`), the same card shows a bar for each, and a silent ongoing
notification carries the same bar, refreshed once a second by a thread of
the node (`watch_fetches`) and taken down when the fetch ends. An APK sent
from a computer was otherwise invisible on the phone until the installer
opened. A waiting application build is named by its APK's label and icon
(`UpdateOffer.label`/`icon`, through `agent::set_looker`), not its id.

The notification is not the only way to the installer: swiped away, or the
installer backed out of by a mis-tap, it used to leave nothing to press. The
control status lists every build waiting for the person (`waiting_builds`,
control v22: Cursio's own and each application's, from `Agent::waiting`,
empty on a computer), and the app's status page shows each with an Install
button (`PhoneUpdateCard`). The button goes `mobile_install_build` →
`cursio_android::waiting_apk` (the ready version's APK) →
`MeshPlugin.installBuild`, which opens the installer from the activity in
front with the same intent the notification carries. When "Install unknown
apps" is still off for Cursio it opens that setting first and says so, and
the installer follows only if the person allowed it; Cursio never changes
the setting itself. Each time the window comes back to the front
(`mobile_setup`) the notification of every waiting build is posted again,
silently after the first. `ready` stays meaningful: it is dropped once the
running version equals it, and a phone whose app restarted before the
person installed recovers it from `versions/` and the store
(`Agent::recover_ready`) — the store already held the statement, so later
offers of that build were old news and nothing would offer it again.

Every APK is signed with the building machine's debug key, so builds from one
machine install over each other; a build from another machine would be refused
by the installer as a different signer.

Other applications' APKs take the same path through the `apps` module, under
`data_dir/apps/<id>/` with a notification per application, and are checked
against the installed package's signer before they are offered; see "Phones"
in `docs/app-sdk-design.md`.

A phone has no shell for `cursio update trust-builder`. So a refusal because
the builder is not trusted carries a button, "Accept builds from <machine>",
which is `ConfigEdit::TrustBuilder`: the same pinned key and the switch to
`dev` the command makes, on every machine, not only phones. The phone that
first runs this needs one install by hand (adb), since a build without the
update module cannot take one.

**macOS** needs a signed and notarized `.app`. Replacing a bundle in
`~/Applications` is allowed; replacing one that lost its signature trips
Gatekeeper and resets the Accessibility and Screen Recording grants, which are
tied to the signing identity. Until builds are signed with a stable identity,
every update costs the user those two permission prompts again, so macOS stays
on manual updates until then.
