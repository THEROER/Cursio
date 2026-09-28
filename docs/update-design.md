# Updating Cursio

How a machine in the mesh gets a new build, switches to it, and gets back off
it when it is bad. Phase 1 (the local half) is built; phases 2–4 are the
design they will follow and are **not built yet**.

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
without a console, so it terminates the process. Phase 2 replaces this with a
request over the control socket that lets the node choose its moment.

## Phase 2: delivery over the mesh (not built)

The mesh is already a trusted, encrypted channel between exactly the machines
that need the build. An update should ride it rather than SSH, and should not
need the internet.

**Offer, then pull.** A node with a newer build announces it (version, channel,
per-platform manifest); a peer that wants it pulls the files at `Bulk`
priority, so a 30 MB executable queues behind the cursor like any file chunk.
A node offers only builds it holds for the peer's platform — Nocturne cannot
build a macOS binary, so it offers the Windows one it staged and nothing else.

**The exchange must survive version skew.** Today a `PROTOCOL_VERSION` mismatch
closes the link, which means precisely the machine that needs an update cannot
hear about it. The update exchange is therefore frozen: a tiny vocabulary
(`have <version> <platform> <manifest digest>`, `want`, chunks) that never
changes shape, carried when `Hello` versions disagree instead of a hard close.
A mismatched link carries that and nothing else.

**Signatures, not trust in whoever sent it.** A manifest is signed; a peer
verifies before installing. Two keys:

- the **release key** (Ed25519), whose public half is compiled into every
  build — what GitHub releases and the `stable` channel are signed with;
- a **builder key**: a machine's identity key that the user explicitly marks,
  on each receiving machine, as allowed to publish to the `dev` channel.
  `cargo xtask publish` signs with it. Being a paired peer is not enough —
  pairing grants input and files, not the right to replace the program.

**Applying waits for a quiet moment.** A node applies only when:

- the cursor is at home (no peer is being driven through this machine),
- no program has a call outstanding on a mounted peer volume and none of our
  volumes is being written by a peer (a restart mid-write is how Steam wedged
  itself on a WinFsp mount),
- no AirPlay session is running on this node.

Before the swap it unmounts peer volumes and tells peers it is leaving, so they
mark it offline rather than waiting for the heartbeat to time out.

**Policy** per machine, in `config.toml`: `update = "auto" | "ask" | "never"`
and `channel = "stable" | "dev"`. `ask` shows the offer in the app ("0.4.3 from
Nocturne") with its changelog and a button; `auto` waits for the quiet moment.

**What it replaces.** `cargo xtask deploy` (SSH, scp, a scheduled task, a
hashed file name per build) becomes `cargo xtask publish`: stage, sign, and
offer to the mesh. Lightmoon updates itself. `deploy` stays until this works on
both machines.

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
