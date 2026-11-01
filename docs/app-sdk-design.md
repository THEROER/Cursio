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
cursio apps launch Lightmoon leavepulse --restart -- ui --no-daemon
cursio apps ship leavepulse --to Lightmoon windows-x86_64=target/.../leavepulse-launcher.exe
```

Arguments after `--` cross with the request (at most 32, each at most 1 KiB):
what runs is still only a file of the installed build, so they choose a mode
of the application, never another program. Windows gets them as one command
line quoted the way its C runtime splits it back apart.

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
| Android | the APK is handed to the system installer through the phone's notification, one per application, and the app's window lists it with an Install button (`crates/android/src/node/update.rs`); see "Phones" | the person taps |

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
`tracing` file.

**When the node cannot open the application's log** — an Android app writes
in a private directory no other app may read, and some applications keep no
log file — the lines come over the SDK's connection instead
(`ClientMsg::Log`, built): `Cursio::log(level, target, message)`, or
`cursio_sdk::tracing_layer(&cursio)` behind the SDK's `tracing` feature,
which formats each event as `tracing`'s default formatter does (RFC 3339 UTC
first). Neither ever waits: lines go on a queue of 1000, sent in batches every
250 ms or at 64 lines; when the application logs faster than they leave, the
oldest are dropped and the node writes how many where they were. The queue
outlives a connection, so a start's lines from before the node answered
arrive. The node keeps them in `<data>/apps/<app>/logs/<process|main>.log.
<date>` (`core/src/apps/app_logs.rs`): daily files named as Cursio's own, 14
days kept, at most 64 MiB a day per process — past it one line says so —
lines cut at 8 KiB with their breaks flattened. The registry records that
file as the process's log, kept across reconnects, so every reader above takes
it unchanged, including after the application exits. A process that also
named its own directory (`App::logs`) is read from there; the lines it sends
are not kept twice.

**Several processes, one application.** A launcher's window and its daemon
both log, and both are the one application `leavepulse` — one build, one
`on_update`. Each registers under that id with its own process name
(`App::new("leavepulse").process("daemon")`; none is the main one), and the
registry keeps one entry per `(app, process)`. `--app leavepulse` merges every
process's log by time, each line marked after its timestamp (`[daemon]`,
`[main]`) so it still merges across machines; `--app leavepulse/daemon` reads
one. A hello without a process is the main one, so the field costs old SDKs
nothing.

Crashes: an application that installs `cursio_sdk::crash::arm()` gets
Cursio's own treatment — the previous run's panic, Windows CrashDumps for its
executable, its own frames only — reported once into its log and its report.

### 3. Commands and messages between copies of one application

**Actions — built (SDK 0.2).** A process declares what another machine may
ask it to do, each with a sentence and a JSON Schema of its arguments:

```rust
App::new("leavepulse")
    .action("launch", "Launch an instance", json!({"type": "object",
        "properties": {"instance": {"type": "string"}}, "required": ["instance"]}),
        |args| launcher.launch(&args["instance"]).map(|pid| json!({"pid": pid})))
```

```sh
cursio apps actions Lightmoon leavepulse
cursio apps call Lightmoon leavepulse launch '{"instance":"vanilla"}'
cursio apps call Lightmoon leavepulse/daemon running
```

- The declarations ride the hello (`ActionSpec`, left out when there are
  none). A call is `NodeMsg::Call { id, action, args }` down the connection
  the process already holds, answered by `ClientMsg::Answer { id, ok | error
  }`; the node (`core/src/apps/{actions,serve}.rs`) correlates by id, answers
  a call whose process closes at once, and gives up after 25 s — under the
  asking side's 30 s for any diag answer, so a slow action reads as slow.
- What runs is the process that is already running, on the SDK's blocking
  pool and never the connection's thread; a handler that panics answers
  `action <name> panicked: <message>` and later calls are served. A wrong name
  answers `<app> has no action <x>; it has: <sorted names>`, in the same words
  from the SDK and the node (`no_such_action`), so a caller learns the right
  name from the error.
- A declaration that cannot stand — an action, topic or process name outside
  lowercase letters, digits and `-`, a repeat, one past the limit — is
  refused on its own: a debug build panics where it is written, a release
  build leaves it out, registers the rest, and lists it in
  `Cursio::rejected()`. `App::start` fails only for the application id or the
  log location. One mistyped `game_log` used to fail `start` and take the
  whole application — logs, presence, every other action — off the mesh
  without a word.
- Over the mesh it is `DiagRequest::AppActions` / `CallAction`, JSON as text
  (the diag vocabulary is bincode). MCP: `apps_actions` (reads), `apps_call`
  (destructive).
- **Its own consent**: `[apps.<id>] actions = "ask" | "allow" | "deny"`,
  `ask` until the machine's person presses **Allow Nocturne to call
  leavepulse's actions** (or Deny). Trusting builds does not extend to it:
  a build is installed and started by the person, an action does something
  the moment it is called.

**Messages between copies — built (SDK 0.3).** The base for an application
keeping its copies on several machines in step — the launcher's instances and
builds across a desk — without a server of its own:

```rust
let cursio = App::new("leavepulse")
    .on_message("instances", |m| merge(&m.from_machine, &m.payload))
    .start()?;
cursio.send("Lightmoon", "instances", &bytes)?;        // blocks until taken
cursio.send_all("instances", &bytes)?;                 // every peer listening
cursio.peers()?;                                       // where it is registered
```

```sh
cursio apps send Lightmoon leavepulse instances 'hello'   # by hand
cursio apps peers leavepulse
```

- **Delivered at most once, to the copies running now; nothing is kept.** A
  failure says which case it was — `Offline` (the machine is not connected),
  `CopyNotRunning` (no copy there listens to the topic), `NotAllowed` (its
  person has not allowed this machine) — and whether trying again means
  anything is the application's call: only it knows whether a late message
  still means anything.
- **At most 512 KiB a message** (`MAX_PAYLOAD`); a build or a world belongs to
  builds or files, a message says what changed. Over 16 KiB it rides the bulk
  class, under it metadata; never input.
- **Local socket:** the hello lists `topics`; `ClientMsg::Send { id, to,
  topic, payload }` (base64) is answered by `NodeMsg::Sent { id, error? }`,
  `ClientMsg::Peers { id }` by `NodeMsg::Peers`, and a message arrives as
  `NodeMsg::Message { from, from_process, topic, payload }` — sent only to a
  process listening to that topic, so an SDK that predates messages never
  meets one (`MAX_LINE` grew from 64 KiB to 1 MiB for them).
- **Between nodes** (`core/src/apps/mesh.rs`): the `apps` module's envelope
  carries `AppsWire::{Datagram, Delivered, Present}` behind the magic `CAPW`,
  beside update-lane messages (`CUPD`). The module's version stays 1 —
  peers run a module only on an exact match, and a bump would have stopped
  builds crossing between an updated machine and one not yet updated; a node
  that predates it finds no lane message in such a payload and skips it.
- **Which machines run it is pushed:** each link's module sends what is
  registered on its machine (`Present`) whenever the registry's generation
  moves, so `peers()` answers from what is already there.
- **Consent is the actions' one.** A machine hands its copy a message only from
  a machine its person lets drive that application (`[apps.<id>] actions`):
  a message is input to a running program exactly as a call is, and a second
  consent for the same trust would be a second question with one answer. No
  listener there is checked first, so nobody is asked about an application
  nothing listens for.
- MCP: `apps_peers` (reads), `apps_send` (destructive).

### Files by their content (built, SDK 0.4)

Messages stop at 512 KiB; a modpack's mods and a world do not. Copies of an
application move files by **content hash**, the way its own manifest already
names them (Modrinth and CurseForge carry sha1; sha256 and sha512 work too):

```rust
let cursio = cursio_sdk::App::new("leavepulse")
    .on_blob(|hash| library.path_of(hash))   // the file holding it, or None
    .start()?;
let report = cursio.fetch(&manifest, &cache_dir, |p| bar.set(p.received, p.size))?;
// report.fetched / report.not_found (get those from the CDN) / report.failed
```

- **The node reads only what the application names.** A peer asks by hash;
  the node asks its running copy (`NodeMsg::BlobPath`) which file holds it and
  reads that one — never a path off the wire. The answer is remembered for the
  chunks that follow.
- **Who holds what.** The fetching node asks every connected machine where the
  application runs which hashes it holds (`AppsWire::BlobHave`), then takes
  each blob from one holder, spreading the bytes across them, machines in
  parallel.
- **Chunks, a window, the bulk class.** 256 KiB per `BlobWant`, at most eight
  outstanding per blob, on the bulk class: a LAN link stays full and a pointer
  moving meanwhile is not held up. Written to disk as they come; nothing holds
  a blob whole in memory (up to 4 GiB each).
- **Nothing counts until it hashes right.** Chunks land in
  `<dest>/<algo>-<hex>.partial`; the whole file is hashed, then renamed to
  `<dest>/<algo>-<hex>` (`BlobHash::file_name`). A mismatch deletes it. A
  partial file left by a dropped link is resumed from its last whole chunk.
  A blob already in place is checked, not fetched.
- **Not found is an answer.** What no connected copy holds comes back in
  `FetchReport::not_found`; where else to get it — a CDN, the cloud — is the
  application's to know, so Cursio never decides it.
- **Consent is the actions' one** (`[apps.<id>] actions`), on the serving
  machine: files leave only to a machine its person lets drive the
  application, as messages arrive only from one.

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
   a notification the installer would refuse after the tap. The
   notification is not the only way: every waiting build is in the
   status's `waiting_builds` with its `app`, and the Cursio app's status
   page offers an Install button for each, which opens the installer from
   the window (or first the "Install unknown apps" setting, saying so);
   see "Phones" in `docs/update-design.md`. An application's build stops
   waiting when the window comes back and its package is installed at the
   APK's version code or later (`CursioNative.apkInstalled`) — the phone
   has no running copy of it to report a version — and each one still
   waiting gets its notification posted again then.
   `cursio apps launch <phone> <app>` **opens** the application
   (`core/src/apps/phone_open.rs`): the package opened is the one the
   phone's person allowed as that id (step 6's `package_access`), never one
   the request names, and no build from the mesh is needed — it may have been
   installed any other way. Android 10+ refuses activity starts from the
   background except for an app whose accessibility service the system has
   bound, so with Cursio's touch service on the launcher screen is started
   directly (`CursioNative.openPackage` → `AppOpener.kt`); without it the
   person gets an "Open <label>" notification and the answer says so.
   MIUI/HyperOS can still drop a direct start silently unless Cursio may
   "display pop-up windows while running in the background" — the answer
   names that setting; nothing changes it. A requested version that differs
   from the installed versionName is reported (the build may still be at the
   installer). `--restart` only opens: one app cannot end another without
   root. `ship`'s waiting (`start=false`) still answers about the build
   offered to the installer. Logs are served only if the app writes into a
   directory Cursio can read (its `Android/media/<package>/` folder, under
   Cursio's all-files access).
2. **SDK on Android (built, step 6).** The node listens on the abstract
   socket `@cursio-apps` (`cursio_app_protocol::ABSTRACT_NAME`;
   `crates/android/src/node/apps.rs`), and `cursio-sdk` connects there on
   Android with the same API — no JNI on the app's side. An abstract socket
   has no file to protect and any app can connect, so each connection passes
   a gate (`core/src/apps/access.rs`): the peer's uid from `SO_PEERCRED`
   (the kernel's, not the app's word), the packages Android says run as it
   (`CursioNative.packagesForUid`, which needs `QUERY_ALL_PACKAGES` since
   Android 11), and the person's answer kept as `package_access` in the
   config. An unknown package raises *"<label> (<package>) asks to talk to
   Cursio as <app>"* with *Let … talk to Cursio* / *Don't*; until then its
   hello is refused with that reason, so the SDK retries rarely. A package is
   taken only under the application id it was allowed as, and an id once
   allowed for one package is refused to every other: otherwise any app could
   register as `leavepulse` and receive its messages and calls. A bound
   service was the alternative; it would have put Kotlin on the app's side of
   every SDK user and a signature permission that only the developer's own
   key could hold.

## Order of work

1. `cursio-app-protocol` + the node's app socket: register, health, logs
   (`app:<id>` in `diag`). Smallest useful slice: remote logs of the launcher.
2. The `apps` module: per-app builds, per-app trust with its button,
   desktop install/switch. **Built**; running the NSIS installer is not.
3. `cursio-sdk` client + `dev::ship`/`logs`/`status`; the launcher's
   `run mesh:<machine>`.
4. Phones: install by notification for apps (reusing Cursio's own path).
   **Built.**
5. Commands and messages between copies. **Built** (actions in SDK 0.2,
   messages in 0.3).
6. SDK on Android. **Built** (0.3.1): the abstract socket and the package
   gate.
