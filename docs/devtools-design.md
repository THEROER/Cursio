# Inspecting and driving an application from another machine

Status: rung 4 is **built on Windows** (UI Automation) **and Linux**
(AT-SPI); rung 5 is **built for CDP** (Chromium web views). WebKitGTK
through WebDriver is **design**.

A person — or an AI agent through `cursio mcp` — wants to work with an
application running on another machine of the mesh the way they would with
one in front of them: read its interface, press its buttons, see its console
and network, run a check in it. Cursio already gives four ways in, each
stronger than the last; this document adds the strongest, places all five on
one scale, and says what each costs in trust.

## The ladder

| Rung | What crosses | Reach | Consent on the machine it reaches |
|---|---|---|---|
| 1. Logs and reports | the app's own log lines | read only | registering with the SDK |
| 2. Actions (`apps call`) | a named action the app chose to expose | what its handler does | `[apps.<id>] actions` |
| 3. Window (`cursio window`) | screenshots, clicks, keys | anything a person at the screen can do | `[apps.<id>] actions` |
| 4. UI tree (`window_tree`, new) | the window's accessibility tree; invoke an element by role and name | what a person can do, addressed by meaning | `[apps.<id>] actions` |
| 5. DevTools (new) | the web view's DevTools protocol | **any code inside the app**: its storage, cookies, tokens | `[apps.<id>] devtools`, separate, time-limited |

Each rung exists because the one below cannot do the job: actions only reach
what the app chose to expose, clicks on a screenshot break when the layout
moves, and a UI tree cannot read the console or the network. An agent should
use the lowest rung that answers, and the tools steer it there: the MCP
descriptions name the rung and its cost.

## Rung 4: the UI tree, for any application

Clicking at fractions of a screenshot is what a person does, and it is the
most brittle thing an agent can do: one changed layout and the click lands
elsewhere, silently. Every desktop already publishes what is on screen as a
tree for screen readers: UI Automation on Windows, AT-SPI on Linux (GTK, Qt,
WebKitGTK and Chromium web views all speak it), NSAccessibility on macOS.

- `window_tree <machine> <app>`: the window's elements as roles, names,
  states and bounds — the shape of Playwright's ARIA snapshot, a few
  kilobytes where a screenshot is a megabyte.
- `window_invoke <machine> <app> --role button --name "Play"`: the element's
  own action (UIA `InvokePattern`, AT-SPI `Action`), no coordinates. Text goes
  in with `ValuePattern`/`EditableText`, not keystrokes, so any script works.
- It needs nothing from the application and works for native windows as well
  as web views; it is the default way an agent drives an interface.
- Same consent as rung 3: it reaches exactly what clicks reach, more reliably.

**Built (Windows).** `cursio window <machine> <app>[/process] [--title T]
tree [--depth N] [--max N] | invoke --role R --name N [--nth K] [--id ID]
[--action invoke|toggle|select|expand|collapse|focus] | set-value … --value V`;
MCP `window_tree` (read-only), `window_invoke` and `window_set_value`
(destructive). Wire: `DiagRequest::WindowTree` / `WindowElement`, appended
last.

- `cursio_window::ui_tree` is the platform-neutral half: the tree, its text
  form (`- button "Play" = value [focused] @0.85,0.12 #42-7-1`, the centre
  as fractions of the window, nameless one-child groups folded away), and
  finding an element — exact name ignoring case, else the only one
  containing it; a miss lists the names it could have meant.
- `cursio_window::windows_uia` walks the control view on a thread of its own
  in the multi-threaded apartment, each child list fetched with a cache
  request for the shown properties (one cross-process call per element, not
  per property). Ids are UI Automation runtime ids; acting re-walks the tree
  (at the request maximum) to find the element by id or by role and name, so
  nothing is held between requests.
- Acting uses the element's own pattern (Invoke, Toggle, SelectionItem,
  ExpandCollapse, Value, `SetFocus`). An element that takes clicks but
  publishes no Invoke pattern is clicked at its centre, as a person would,
  and the answer says so.
- Limits: 40 levels and 3000 elements by default, at most 64 and 10 000.
- Not checked live: a running WebView2 page, which waits on a deployed
  build.

**Built (Linux).** The same commands, tools and wire, answered by
`cursio_window::linux_atspi`:

- The accessibility bus's address comes from `org.a11y.Bus.GetAddress` on
  the session bus; the registry root lists applications as bus names, and
  the one the request means is the one whose owner pid
  (`GetConnectionUnixProcessID`) is among the pids it registered with. Its
  top-level accessible whose name contains `--title` is the window, else
  the active one, else the first on screen. The Shell extension is not
  involved.
- Each element is `GetRoleName` (names, not numbers, which toolkits number
  differently), `Name`, `GetState`, `GetInterfaces` and, when showing,
  `Component.GetExtents` in *window* coordinates divided by the frame's size
  — screen coordinates mean nothing to a Wayland client. Roles and states
  map onto UI Automation's vocabulary (`push button` → `button`, `page tab`
  → `tabitem`; disabled is neither ENABLED nor SENSITIVE, since GTK 4 sets
  only the latter); defunct elements are skipped. A value is `Value`'s
  number, or the text of an entry, never of a password field or a document.
- Ids are the last part of the object path (GTK 4 uses UUIDs, so they are
  long). Acting re-walks, as on Windows.
- Invoke, toggle, select, expand and collapse run the element's
  `Action` by the names toolkits give it (`click`, `press`, `activate`,
  `toggle`, `expand or contract`, …); a miss lists the actions it does
  offer. Expand on an expanded element answers so instead of toggling it
  shut. Focus is `Component.GrabFocus`; set-value is
  `EditableText.SetTextContents`, else `Value.CurrentValue`.
- Every call runs on a thread of its own (zbus's blocking calls cannot run
  inside the node's runtime) with a 2 s method timeout, so a hung
  application fails the request instead of holding it.
- GTK registers whether or not assistive technology is on; Chromium and
  Electron publish only if `org.a11y.Status.IsEnabled` was true when they
  started, or with `--force-renderer-accessibility`. An application missing
  from the bus while it is off is answered with how the person turns it on
  (`gsettings set org.gnome.desktop.interface toolkit-accessibility true`,
  then restart the application); Cursio never changes the setting.
- Checked live on Nocturne, read only, with
  `cargo run -p cursio-window --example atspi_tree -- <pid> [title]`:
  Nautilus' window, 400 elements in about a second (six D-Bus calls per
  element); the snap Firefox, with the bus off, gets the message above.

## Rung 5: DevTools

### What the application does

Only a process started with `--devtools` (the application's own flag, which a
release build may compile out) opens a DevTools endpoint, and only on
loopback:

- Chromium web views (WebView2, so Tauri on Windows; Electron; CEF) speak the
  Chrome DevTools Protocol (CDP). The application picks a free loopback port
  itself and sets it before its web view is created
  (`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=N`).
- WebKitGTK (Tauri on Linux) has no CDP and its WebDriver cannot attach to a
  window that is already open; it is a different mode (the app started under
  the driver) and comes later.

It declares the endpoint through the SDK: `App::devtools(Devtools::Cdp { port })`.

### What the node checks

- The port is listened on by **this process** (the socket table's owning
  pid is one the app registered). Otherwise any program could register any
  local service — a database, another app's debugger — and have the mesh
  carry it.
- The endpoint is loopback; nothing else is forwarded.

### Consent: the strongest on the ladder

CDP runs any JavaScript in the application. It reads what the application
holds: local storage, cookies, session tokens. So:

- `[apps.<id>] devtools = "deny"` by default. `ask` raises a problem with a
  plain warning: *"Nocturne asks to inspect leavepulse. It could run any code
  inside it and read what it stores, including signed-in accounts."*
- The answer is **time-limited**: *Allow for 1 hour* and *Allow until
  leavepulse restarts*. There is no "always". Trusting a machine with an
  application's builds or actions does not grant this.
- Per requesting machine: allowing Nocturne does not allow dark.
- **Visible while it lasts**: the machine shows that it is being inspected
  (a notification with *Stop*, the tray, the app's status page), and every
  session is logged — who, when, for how long — so `cursio report` shows it
  afterwards.
- *Stop* or the time running out closes every tunnel at once.

### The tunnel

- A stream carried by the `apps` module: open, data, close, each stream
  numbered. Small frames ride `Metadata`, large ones `Bulk`; never `Input`.
- On the asking machine, `cursio devtools Lightmoon leavepulse` listens on
  `127.0.0.1` only, for as long as it runs.
- **A token in the path.** The forwarded port is reachable by every local
  user of the asking machine, and CDP has no authentication of its own. The
  listener answers only `ws://127.0.0.1:<port>/<token>/devtools/…` and
  `http://127.0.0.1:<port>/<token>/json…`, strips the token and forwards the
  rest. The command prints the full URL; without the token the port is dead.
- Chrome builds `webSocketDebuggerUrl` from the request's `Host` header, so
  answers to `/json/version` already point back through the tunnel; the
  tunnel inserts the token into those URLs. Chrome's DNS-rebinding guard
  accepts only `localhost` or an IP as `Host`, which `127.0.0.1` is.

### What it gives, without Playwright

Playwright's `connectOverCDP(url)` and the chrome-devtools MCP server work
against the printed URL as against a local browser. An agent should not need
either, so `cursio mcp` speaks CDP itself for the common cases:

| Tool | CDP | Cost |
|---|---|---|
| `web_snapshot` | `Accessibility.getFullAXTree` | read only |
| `web_query` | `DOM.querySelectorAll` + outer HTML / text | read only |
| `web_screenshot` | `Page.captureScreenshot` | read only |
| `web_console` | `Runtime.consoleAPICalled`, exceptions | read only |
| `web_network` | `Network.*` (URLs, status, timing; bodies on request) | read only |
| `web_click`, `web_fill` | resolve a selector, dispatch input | changes state |
| `web_eval` | `Runtime.evaluate` | **runs code**; its own MCP annotation |

The web view's console also becomes a log: while a machine is allowed to
inspect the app, `cursio logs Lightmoon --app leavepulse/console` reads it,
merged by time with the app's own log.

### Built (CDP)

- **SDK 0.5.0** (`cursio-sdk`, `cursio-app-protocol`):
  `App::devtools(Devtools::Cdp { port })` puts `"devtools":{"kind":"cdp",
  "port":N}` in `Hello`. The field is omitted when absent, so a 0.4 hello is
  byte for byte what it was (pinned by tests).
- **The owner check** (`core/src/apps/devtools.rs`): the port must be in the
  listening state on loopback and owned by a pid the app registered — Linux
  maps `/proc/net/tcp{,6}`'s inode to `/proc/<pid>/fd`, Windows reads
  `GetExtendedTcpTable` with owning pids. A port listened on everywhere is
  refused with that reason; other platforms refuse outright.
- **Consent**: `[apps.<id>] devtools = "deny" | "ask"`. Under `deny` a
  request raises an information problem whose one button switches to `ask`;
  under `ask` it raises the warning above with *Allow <peer> for 1 hour*,
  *Allow until <app> restarts* and *Deny*. Problem buttons are config edits,
  so a grant is stored as `[[apps.<id>.devtools_grants]]` with the peer, an
  absolute expiry and, for *until it restarts*, the pids it was given to (and
  an expiry of 24 hours at most). A grant past its expiry, for another
  machine or for pids no longer running holds nothing; `deny` clears every
  grant. Every open stream rechecks its grant every 3 seconds, so *Stop* or
  the expiry closes them all.
- **Visible while it lasts**: an open session raises a warning problem
  *"<peer> is inspecting <app>"* with *Stop* (which clears the grants), and
  logs `devtools session began` / `devtools session ended … for N s` at
  WARN, which `cursio report` counts with the day's warnings. The session
  ends a minute after its last stream closes, since a browser reconnects.
- **The tunnel** (`core/src/apps/tunnel.rs`): `AppsWire::TunnelOpen`,
  `TunnelOpened`, `TunnelData`, `TunnelAck`, `TunnelClose`, appended last.
  Data goes in frames of at most 32 KiB against a 256 KiB credit window that
  the far end returns as it writes, so a stalled reader stops the sender
  rather than filling a queue; small frames ride `Metadata`, large ones
  `Bulk`. A link going away closes its streams.
- **The asking side** lives in the node, not the command, so the MCP tools
  reuse one listener: `cursio devtools <machine> <app>[/process] [--port P]
  [--close]` asks the far side once (so a refusal is an error at once),
  listens on `127.0.0.1`, and prints the URL with its token, a Playwright
  line and the frontend hint. A request whose path does not start with the
  token is answered 404 and never forwarded; the token is stripped, `Host`
  becomes `127.0.0.1:<port>`, and `/json*` answers get the token inserted
  into `webSocketDebuggerUrl` and `devtoolsFrontendUrl` with
  `Content-Length` fixed. The listener closes after 15 minutes idle.
- **MCP**: `apps_devtools` (destructive) opens the listener; `web_snapshot`,
  `web_query`, `web_screenshot`, `web_console`, `web_network`, `web_click`,
  `web_fill` and `web_eval` (destructive, with its own title) speak CDP to
  the first page target through it (`core/src/mcp/cdp.rs`, tokio-tungstenite
  behind the `web-tools` feature).
- **`web_network`**: every tool call is a CDP session of its own, so it
  enables `Network` and records what the page requests while it waits
  (`wait_ms`, 2 s by default, 15 s at most): method, status, type, time,
  wire size and URL, the newest `max` (100) rows of at most 4000 events, a
  redirect as its own row. What the page fetched *before* the call comes
  from its performance timeline (URL, initiator, time, size, status where
  Chromium reports it), since a fresh session has no record of it. A body is
  fetched only when asked — `body_for` names part of a URL, and the newest
  finished request matching it is read with `Network.getResponseBody`, cut at
  64 KiB, binary bodies only measured. Read only.
- **The console as a log** (`core/src/apps/devtools_console.rs`): the first
  stream a granted machine opens into an application starts, on the owning
  machine, one watcher per application. It connects to the page straight on
  the loopback port (no tunnel, no token), enables `Runtime` and `Log`, and
  writes every console call, uncaught exception and browser log entry
  through the node-kept app logs as process `console` — the page's own time
  in UTC to the microsecond first, then `ERROR`/`WARN`/`INFO`/`DEBUG`, then
  the line `web_console` would show. The registry records that log like a
  process's, so `cursio logs Lightmoon --app leavepulse/console` (or the
  whole app, merged) reads it during the session and after. The watcher
  checks every 3 s that one of the machines it serves still holds a grant
  and stops with *Stop* or the end of the hour; it reconnects while allowed
  when the page goes away. `Runtime.enable` replays earlier messages, so the
  newest page time written per application is remembered and nothing at or
  before it is written twice.
- **Not built**: `--launch`, discovery of rungs in `apps status`, a notification and tray
  entry beyond the problem, WebKitGTK through WebDriver, and a live run
  against a deployed WebView2 application.

## Convenience

- **One command, ready to use.** `cursio devtools Lightmoon leavepulse`
  prints the URL, a Playwright snippet, and a DevTools front-end link that
  opens the inspector in a local Chromium. `--launch` starts the application
  with `--devtools` first (`apps launch … -- --devtools`) when it is not
  running so.
- **Discovery.** `cursio apps status` and MCP `apps_peers` show, per process,
  which rungs it offers: actions by name, window, UI tree, DevTools.
- **One mental model for agents.** Every tool takes `machine`, `app[/process]`
  and returns the same error vocabulary: not running, not allowed (with the
  button it raised), offline.

## What an agent can and cannot do

The mesh is the user's own machines, paired with a password, so a peer is
trusted with input and files already. What this design adds on top:

- Nothing reaches an application its machine's person did not register
  (SDK), allow (actions), and, for DevTools, allow again for a limited time.
- No rung reaches the operating system: there is no shell, and DevTools
  reaches only the web view of an application that started with
  `--devtools`.
- Claude Code's own permission prompts still apply to the MCP tools marked as
  changing state; a user who wants an agent to ask first for `web_eval`,
  `apps_call` or `window_click` keeps them out of the allowlist.

## Order of work

1. Rung 4, the UI tree: UI Automation on Windows and AT-SPI on Linux
   (**built**).
2. Rung 5's tunnel with the token, the time-limited consent, the visible
   indicator and the audit line (**built**).
3. The `web_*` tools in `cursio mcp`, and the web view's console as a log
   (**built**).
4. WebKitGTK through WebDriver, as its own mode.
