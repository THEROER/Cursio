# Inspecting and driving an application from another machine

Status: rung 4 is **built on Windows** (UI Automation); AT-SPI on Linux and
rung 5 are **design**.

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
- Not built: AT-SPI on Linux (the answer says so; screenshots and clicks
  work there through the Shell extension) and live checks against a running
  WebView2 page, which wait on a deployed build.

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

The web view's console also becomes a log: `cursio logs Lightmoon --app
leavepulse/console` while a DevTools session is open, merged by time with the
app's own log.

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

1. Rung 4, the UI tree: UI Automation on Windows (**built**), AT-SPI next.
2. Rung 5's tunnel with the token, the time-limited consent, the visible
   indicator and the audit line.
3. The `web_*` tools in `cursio mcp`, and the web view's console as a log.
4. WebKitGTK through WebDriver, as its own mode.
