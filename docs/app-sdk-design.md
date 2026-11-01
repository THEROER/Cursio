# Applications on the mesh (`cursio-sdk`)

Status: step 1 of "Order of work" is built — `crates/app-protocol`,
`crates/sdk` (`App::new(..).version(..).logs(..).start()`, health), the
node's apps socket and registry (`core/src/apps`), and `cursio logs --app`.
The rest is design.

## The problem

Cursio already solves, for itself, everything a developer with several
machines keeps solving by hand for every other program they write:

- getting a fresh build onto the laptop and the phones (signed, resumable,
  with rollback — `docs/update-design.md`);
- reading what that build logged on a machine you are not sitting at
  (`cursio logs`, `cursio report` — "Every machine's log" in `live-test.md`);
- knowing which machines are up, on what path, on what battery;
- restarting a thing remotely, choosing a path, holding a session across a
  dropped connection.

The LeavePulse launcher is the first case: it builds for Windows, Linux and
Android from one `cargo xtask`, but reaches Lightmoon only by carrying an
`.exe` over by hand, reaches a phone only over USB (`run adb:SERIAL`), and its
self-update reads only the cloud update service — so a local build cannot be
tried on another machine without publishing it.

Teaching Cursio about each program by name is the wrong fix: the CLI's rule
already is "add capability by adding a module, not by branching". The fix is
the same rule one level up — **a program becomes a participant of the mesh by
linking a small library, and Cursio carries for it what it carries for
itself.**

## What it is not

- **Not a second mesh.** An application never opens its own Noise session,
  never pairs, never holds a key. The node on that machine is the only mesh
  member; the application is its client. Two meshes on one machine would mean
  two trust stores that disagree, which is the failure `claim_link` exists to
  prevent between two links.
- **Not production fleet management.** LeavePulse's `fleet-service`,
  `control-agent` and `deploy-toolkit` run servers over WireGuard and gRPC with
  enrolment and RBAC. This is the developer's own desk: machines one person
  paired with a password, LAN and Bluetooth only. The two must not grow into
  each other; if a server ever needs a build, that is fleet's job.
- **Not remote shell.** Nothing here runs an arbitrary command on another
  machine. An application exposes named commands it chose to handle; a build
  is installed only for an application that machine agreed to take builds of.

## Shape

```
 machine A                                   machine B
┌──────────────────────┐                   ┌──────────────────────┐
│ leavepulse-launcher  │                   │ leavepulse-launcher  │
│   └ cursio-sdk ──────┼─ local socket ─┐  │   └ cursio-sdk       │
│                      │                │  │        ▲             │
│ cargo xtask          │                │  │        │ local socket│
│   └ cursio-sdk::dev ─┼────────────────┤  │        │             │
│                      │                ▼  │        │             │
│              cursio node ══ mesh (apps module, 16) ══ cursio node │
└──────────────────────┘                   └──────────────────────┘
```

Three layers, each in its own crate:

| Crate | License | Holds | Depends on |
|---|---|---|---|
| `cursio-app-protocol` | MIT OR Apache-2.0 | the public local-socket vocabulary, versioned | `serde`, `serde_json` (`libc` on Unix, for the uid) |
| `cursio-sdk` | MIT OR Apache-2.0 | the client: connect, register, handlers, `dev` tools | the protocol crate, `tokio` |
| `cursio-core` (`apps/`) | GPL-3.0-or-later | the node's side: registry of local apps, the apps socket, later the `apps` module | everything |

**The license split is load-bearing.** Cursio is GPL because of `playfair`;
the launcher is Apache-2.0. If the SDK depended on `cursio-core` or
`cursio-protocol`, linking it would put the launcher under the GPL. The SDK
crates therefore depend on nothing GPL, are written fresh rather than moved out
of existing files (moved code would need every contributor's consent to
relicense), and talk to the node over a socket, which is communication between
programs, not linking. The node side may freely depend on the public crate.

## The local socket

The control socket (`core/src/control`) is already the right transport —
JSON per line, request ids, pushed events after `Subscribe`, owner-only Unix
socket or named pipe — but its vocabulary is internal and changes whenever the
app's settings page does. Applications cannot be broken by that.

So applications get **their own endpoint beside it** — `apps.sock` next to
`control.sock` in the node's private runtime directory, or
`\\.\pipe\cursio-apps-<user>` — speaking only `cursio-app-protocol`, and bound
through the control socket's own helpers so the privacy rules are one piece of
code:

- **Versioned like the update lane.** The first message is
  `Hello { protocol, app, version, pid, logs }`; the node answers
  `Welcome { protocol, machine }` with the highest version both know, or
  `Refused { reason }` and closes. Messages are added, never changed; bytes of
  every message are pinned by tests in the protocol crate. A node too old for
  an SDK feature answers `Unsupported { feature }`, never silence.
- **Same privacy as the control socket.** Owner only. Any program of this user
  can register as any app id — that is the same trust a user's programs already
  have over each other's files; the mesh boundary is where trust is enforced,
  not here.
- **No node running is an ordinary state.** `cursio_sdk::App::start` returns
  a handle whose connection lives on its own thread and runtime and
  reconnects with a backoff; it fails only for an invalid id or log path.
  Every call made while disconnected returns `NotRunning` rather than
  blocking, and is kept so the node hears the latest state when it is back. An application
  must work exactly as before when Cursio is absent; linking the SDK is never
  a new way for it to fail to start.

## What an application gets

### 1. Builds, carried by the mesh

The update machinery is reused whole — `Statement`, `Offer`/`Want`, the store,
resumable fetch, signatures, builder trust — with one new dimension: **an
application id**. A Cursio build is the application `cursio`; nothing about
its own path changes, and the update lane itself is untouched (see "Wire
compatibility" in `CLAUDE.md`). Builds of other applications travel in the
`apps` module (16), which carries the same offer/want/chunk shapes keyed by
`(app, version, platform)`.

**Built (step 2).** Each application has its own update agent
(`core/src/apps/builds.rs`), made the first time it is named and found again
on disk: install root `apps/<id>/` under Cursio's, its own store, its own
`[apps.<id>]` in the config (`policy`, `trusted_builders`), its own problems
(`app.<id>.refused`, …). The agent is Cursio's own, given a `Subject`
(`update/agent.rs`); one update `Lane` per application per link rides the
`apps` module (`core/src/apps/lane.rs`), every lane message wrapped in
`AppsMsg { app, update }` (`protocol/src/apps.rs`). Platform names are the
bare `<os>-<arch>` (`windows-x86_64`, `linux-x86_64`, `android-aarch64`):
the FFmpeg suffix is Cursio's own concern.

```sh
cursio apps publish leavepulse --version 0.6.1 \
    windows-x86_64=target/x86_64-pc-windows-msvc/release/leavepulse-launcher.exe \
    linux-x86_64=target/release/leavepulse-launcher
cursio apps status
cursio apps launch Lightmoon leavepulse --restart
cursio apps ship leavepulse --to Lightmoon windows-x86_64=target/.../leavepulse-launcher.exe
```

**Starting it from another machine** (`core/src/apps/launch.rs`) is a diag
request, `DiagRequest::Launch`, so it rides what `cursio restart` does. The
request names an application and a component, never a path: only a build
the `apps` module installed can start, and `--restart` ends only processes
whose executable lies in that application's install root. `ship` is publish,
then launch with the version until the machine answers that it is current —
its refusals ("not current here yet; last: refused …") are the progress.

**As whom.** A build asks for administrator rights per file
(`cursio apps publish --admin <file>`, `Component::admin` in the signed
manifest); the receiving machine answers once per application
(`[apps.<id>] admin = "ask" | "allow" | "deny"`, default `ask`, raised as a
problem with both answers as buttons). Without the ask, or answered `deny`,
it runs as the person — on Windows with the desktop shell's token even
when the node is elevated, since a child of the elevated node would inherit
its token. Answered `allow`: an elevated node starts it directly, an
unelevated one through `runas`, so Windows' own prompt confirms each start;
on Linux through `pkexec`.

`publish` needs no running node: it signs into the application's store with
this machine's builder key (`cursio update key`), and the node offers it to
every link within 30 s. A machine that has not trusted this builder for that
application refuses with **Take leavepulse builds from Nocturne**; pressing it
writes `ConfigEdit::TrustAppBuilder`, and the next offer comes through.

On the receiving machine:

| Machine | Install | Switch |
|---|---|---|
| Windows / Linux | `apps/<app>/versions/<v>/` + `current` + rollback, the `InstallLayout` of `update-design.md`, rooted per app | `current` is pointed at it at once and every running copy hears `NodeMsg::BuildReady` (the SDK's `on_update`); when to restart into it is the application's choice. Starting it is asked for (`cursio apps launch`, below) |
| Windows, NSIS bundle (not built) | the installer is run with `/S` once, its exit code is the result | as above |
| Android | the APK is handed to the system installer through the phone's notification, one per application (`crates/android/src/node/update.rs`); see "Phones" | the person taps |

**Taking builds is per application and explicit.** A machine takes builds of
`leavepulse` only after its person pressed "Take leavepulse builds from
Nocturne" — the same refusal-with-a-button already shown for Cursio builds
(`Untrusted`/`WrongChannel`). Trusting a builder for Cursio does not extend to
other applications, and no application is ever installed on a machine where
it was not already registered or explicitly accepted. This is what keeps
"ship a build" from being "run code on any machine I paired".

**Android signing.** Android replaces an app only with an APK signed by the
same key. A dev build signed with the developer's debug key cannot replace a
store build, so an application shipping to phones builds its dev variant
under its own id (`applicationIdSuffix ".dev"`). The SDK's `dev::ship` refuses
up front, with that explanation, when the APK's signer differs from the one
installed on the phone — the installer would otherwise fail with an error the
person sees and the developer does not.

### 2. Logs and reports, read from anywhere

An application registers its log directory and file prefix. `diag`'s
`valid_log_name` then accepts `app:<id>`, resolving to that directory only —
the node still never reads a path a peer named. `cursio logs Lightmoon --app
leavepulse -f`, `cursio report all --app leavepulse --since 1d` and the merged
`logs all` work unchanged, because `log_tail` and `log_report` read any
`tracing` file. The SDK offers `cursio_sdk::tracing_layer()` so the format
matches without the application adopting `cursio-log`.

Crashes: an application that installs `cursio_sdk::crash::arm()` gets
Cursio's own treatment — the previous run's panic, Windows CrashDumps for its
executable, its own frames only — reported once into its log and its report.

### 3. Commands and messages between copies of one application

- `on_command("name", handler)` exposes a named command; `cursio app
  Lightmoon leavepulse reload-config` or `sdk.call("Lightmoon",
  "reload-config", args)` invokes it. Only the application's own copies and
  the CLI may call; nothing else on the mesh can.
- `send(peer, topic, bytes)` / `on_message(topic, handler)`: a datagram to the
  same application on another machine, over the mesh's encryption, with the
  application choosing a priority class (`Control`, `Metadata`, `Bulk`, never
  `Input`). Lossy or reliable is the application's choice, delivered as the
  existing `Message::is_lossy` rule.
- A request whose peer has the application closed fails with
  `NotRunning { peer }`, distinct from `Offline { peer }`.

### 4. State of the desk

`peers()` and `on_peers` give what `NodeStatus` already carries — online,
path, battery, network — plus which peers have this application registered and
at which version. That last column is what lets a developer see at a glance
that the phone is two builds behind.

## Developer tools (`cursio_sdk::dev`)

The same crate, behind a `dev` feature, used from a project's `xtask`:

```rust
dev::ship(App::new("leavepulse").version(v))
    .artifact(Platform::WindowsX64, "target/.../leavepulse-launcher.exe")
    .artifact(Platform::AndroidArm64, "target/.../app-arm64.apk")
    .to(Target::Machine("Lightmoon")).launch().await?;
dev::logs("Lightmoon", "leavepulse").since("10m").follow().await?;
dev::status("leavepulse").await?;     // every machine: version, running, last crash
```

Signing uses this machine's builder key (`cursio update key`), so a project
needs no key of its own. In the launcher this becomes `cargo xtask run
mesh:Lightmoon` / `mesh:all`, beside the existing `run adb:SERIAL`, and a
`live-test`-style smoke: ship, launch, wait for the app's own `ready` health,
read its log for warnings, PASS/FAIL.

## Phones

The node on a phone lives inside the Cursio app; its sockets are in that app's
private directory, which another app cannot open. Two steps:

1. **Without SDK on the phone (built, step 4).** The phone runs the `apps`
   module like a desktop: builds land under `data_dir/apps/<id>/`
   (`Builds::start_with`, its root chosen per install), per-app trust is the
   same button, and an installed build is handed to the system installer
   (`crates/android/src/node/update.rs` `offer_app_to_installer` →
   `UpdateOffer.offerApp`) as one notification per application, "<app>
   update ready — Tap to install <version>". The build's APK is the
   component named `apk`, else its one `.apk` file. Before offering, the
   phone compares the APK's signer with the installed package's
   (`GET_SIGNING_CERTIFICATES`); a mismatch is raised as
   `app.<id>.signer` with the `applicationIdSuffix ".dev"` advice instead of
   a notification the installer would refuse after the tap.
   `cursio apps launch <phone> <app>` answers that the build is offered to
   the installer and runs once its person installs and opens it — a phone
   starts no process for a peer. Logs are served only if the app writes
   into a directory it shares.
2. **SDK on Android.** A bound service exported by the Cursio app, guarded by
   a signature-level permission the developer's apps hold, or a
   `LocalSocket` in the abstract namespace with the peer's UID checked against
   a list the person approved. Decided when the second step starts.

## Order of work

1. `cursio-app-protocol` + the node's app socket: register, health, logs
   (`app:<id>` in `diag`). Smallest useful slice: remote logs of the launcher.
2. The `apps` module: per-app builds, per-app trust with its button,
   desktop install/switch. **Built**; running the NSIS installer is not.
3. `cursio-sdk` client + `dev::ship`/`logs`/`status`; the launcher's
   `run mesh:<machine>`.
4. Phones: install by notification for apps (reusing Cursio's own path).
   **Built.**
5. Commands and messages between copies.
6. SDK on Android.
