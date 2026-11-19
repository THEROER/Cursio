# The mesh as tools for an AI agent (`cursio mcp`)

Status: **built** — machines, logs, report, path, apps status / publish /
launch / ship, restart, apply-update, and the phone's screen and touch
(`phone_screenshot`, `phone_tap`, `phone_swipe`, `phone_key`, `phone_text`).
Notifications, messages, clipboard and gallery tools are next.

## Why

Working on the laptop meant a password typed into SSH through WSL; working on
a phone meant plugging it in for adb. Cursio already reaches every machine and
phone of the person's, encrypted, without either — the CLI proves it
(`cursio logs`, `cursio report`, `cursio apps ship`). An agent such as Claude
Code should have the same reach, through tools rather than by scraping a
terminal, and with the same consent the person has.

## Shape

`cursio mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
server on stdin/stdout: JSON-RPC 2.0, one message per line (`initialize`,
`tools/list`, `tools/call`, `ping`; notifications unanswered). The client
starts it as a child:

```sh
claude mcp add cursio -- cursio mcp
```

Every tool is a control command the running node already answers, or a
`cursio apps` function (`core/src/apps/cli.rs`, made callable for this:
progress goes to a sink instead of stdout). The shared queries — which
machines `this`/`all` mean, a span like `10m`, a log's full name, merging
several machines' lines by time — live in `core/src/control/query.rs` and are
what `cursio logs` uses too, so the CLI and the agent cannot drift.

The server holds no connection to the node: each call reaches the control
socket afresh (`NodeAsk`), because `apply_update` and `restart_machine`
replace the node it would be holding, and an agent session outlives many of
them. With no node running, every tool answers that, and nothing else breaks.

The server itself follows the installed version the same way. Started once
per agent session, it otherwise stayed the build that was current then, for
days, and failed on what newer builds wrote — a config key it did not know,
a node answering in a newer vocabulary. Before each message it checks what is
current (`mcp::successor`); when that is another build, it starts that build's
`cursio mcp` and passes the lines through to it. The client keeps the one
child it started, which is why it passes lines through instead of exec'ing the
new build: Windows has no exec. Switching mid-session loses nothing, since the
server holds no state between messages.

stdout carries the protocol alone; the log goes to stderr and its file.

## Tools

| Tool | Reads / changes | Maps to |
|---|---|---|
| `machines` | reads | `Command::Status`: every machine, online, kind, battery/network, modules; this machine's problems |
| `logs` | reads | `Command::Logs` per machine, `machine: all` merged by time; `app` reads `app:<id>` (cursio-sdk), `log` another Cursio log |
| `report` | reads | `Command::Report`: a machine's day — drops, warnings, slow modules, memory |
| `path` | reads | `Command::Paths` without a choice |
| `apps_status` | reads | `apps::cli::status` |
| `apps_publish` | changes (offers a build) | `apps::cli::publish` |
| `apps_launch` | changes; destructive with `restart` | `Command::Launch` |
| `apps_ship` | changes; destructive (replaces running copies) | publish, then launch with the version until current there |
| `apps_actions` | reads | `Command::AppActions`: what a running application declared (name, description, JSON Schema, which process, whether running) |
| `apps_call` | destructive | `Command::CallAction`: the running process does one of its declared actions and answers JSON; the target machine's person allows calls to an application once (`[apps.<id>] actions`) |
| `apps_peers` | reads | `Command::AppPeers`: the connected machines where an application is registered — running or not, version, the message topics its copies listen to |
| `apps_send` | destructive | `Command::AppSend`: a text message about a topic to the copies of an application on a machine, as its copy here would send one; fails saying offline, no copy listening, or not allowed (same consent as `apps_call`) |
| `restart_machine` | destructive | `Command::Maintain(Restart)` |
| `apply_update` | destructive | `Command::Maintain(ApplyUpdate)` |

Each carries MCP's annotations — `readOnlyHint`, `destructiveHint`,
`idempotentHint`, and `openWorldHint` (every tool reaches beyond this
process) — so a client can ask its person before a destructive one. A tool
that runs and cannot do it answers a result with `isError` and the node's
reason; only an unknown tool is a protocol error.

## Consent is the target machine's, never the agent's

No tool changes a setting. What a node would ask its person — trusting a
builder for an application, letting an application run as administrator,
showing its screen — stays a button or a prompt on that machine, and a tool
that reaches it answers with the refusal that says so ("this machine takes
leavepulse only from machines trusted for it"). The instructions given at
`initialize` tell the agent to pass that to the user rather than retry. This
is the rule the whole project keeps (no silent settings changes), applied to
a caller that is not a person.

## The phone: screen and touch

What existed: `cursio screen <phone>` / `Command::PhoneScreen` asks the phone
to share its screen (it asks its person — Android's own consent, once per
process); the phone's MediaCodec H.264 arrives as an ordinary transferred
window; the viewer's pointer comes back as `WindowInput` and
`phone_gesture.rs` turns it into touch, which `ScreenTouch.kt` (an
accessibility service the person turns on once) dispatches.

What is built on top — **nothing new crosses the link**
(`window-session/src/phone_screen/remote.rs`):

- **`phone_screenshot { phone, ask?, max_side? }` → MCP `image` content.**
  The computer's window engine for a link to a phone hands the registry the
  screen window's id and size and every access unit from its newest keyframe
  on (bounded at 32 MiB, past which it asks the phone for a keyframe). A
  request decodes that run once with the viewer's own decoder and writes a
  PNG (`phone_screen/shot.rs`, `Command::PhoneScreenshot` →
  `Reply::Screenshot { path }`) under the data dir's `screenshots/`, keeping
  the last ten: a phone's screen as PNG is megabytes, past one control line,
  and every reader is on this machine. The MCP tool scales the long side to
  1280 px unless asked otherwise. Not shared: the answer says to ask for it
  and allow it on the phone; `ask` sends the request and waits up to 45 s.
  The surrogate window opens as it does for `cursio screen` — a hidden
  session was not needed.
- **`phone_tap { phone, x, y, hold_ms? }`, `phone_swipe { phone, x1, y1, x2,
  y2, ms? }`, `phone_key { phone, key: back | home | recents }`,
  `phone_text { phone, text }`.** `window-session/src/phone_action.rs` turns each into
  the `WindowInput` a surrogate would send — points are fractions of the
  picture, so the caller never needs the resolution — and
  `Command::PhoneAct` sends them into the link's window engine, paced (a
  swipe is a move every ~16 ms). Recents is the forward side button, newly
  `Gesture::Recents` → `GLOBAL_ACTION_RECENTS`. All destructive-marked: they
  act in the person's phone UI.
- **Touch says when it cannot land** (`window-session/src/phone_hands.rs`). Touch
  needs Cursio's accessibility service on the phone, which its person turns
  on by hand; with it off Android drops the gesture silently. So before a
  tap, swipe or key the computer asks `DiagRequest::PhoneTouch`, and a phone
  with the service off answers what to turn on, which the tool returns as
  its error. Cursio never turns the setting on itself.
- **Text in any script.** `phone_text` goes as `DiagRequest::PhoneType`: the
  accessibility service commits it through the focused field's input
  connection (Android 13+) or sets the field's text itself (`ACTION_SET_TEXT`),
  so Cyrillic or emoji reach it, and "no text field has
  focus" comes back as an error instead of keys falling nowhere. A phone
  older than these questions drops them unanswered; after 3 s the computer
  falls back to the old way — touch unchecked, text as US-layout keys,
  ASCII only.
- The same as a CLI: `cursio phone <phone> screenshot [--ask] [--out F]`,
  `tap X Y [--hold MS]`, `swipe X1 Y1 X2 Y2 [--ms]`, `key back|home|recents`,
  `text "…"`.
- **`phone_logs { phone, app? }`.** The phone's own Cursio log already reads
  through `logs` (its node keeps a file). Android gives an app no other app's
  logcat (`READ_LOGS` is a system permission), so another application's log
  on a phone reads through cursio-sdk on Android (step 6 of
  `docs/app-sdk-design.md`) — not through logcat.
- Cheap next: `notifications` (the notify module's recent list), `messages`
  (`ListConversations`/`ListMessages`, reads; `SendMessage` destructive),
  `clipboard_get/set`, `gallery_list/fetch`, `ring`.

## A desktop application's window

An application's declared actions (`apps_call`) are the first way to drive it;
its window is the fallback beside them, and what checks what a person sees —
that a button now shows "Running". `core/src/desktop_window.rs`:

- **`window_screenshot { machine, app, title?, max_side? }`** (read-only) and
  **`window_click { machine, app, title?, x, y, right? }`,
  `window_type { …, text }`, `window_key { …, key }`** (destructive). Above
  them, **`window_tree`** (read-only) and **`window_invoke` /
  `window_set_value`** (destructive) read the window's accessibility tree (UI
  Automation on Windows, AT-SPI on Linux) and
  act on an element by role and name — the way an agent should drive an
  interface first; see rung 4 in `docs/devtools-design.md`. The clicks are
  `cursio window <machine> <app>[/process] [--title T] screenshot | click X Y
  [--right] | type TEXT | key K`. Wire: `DiagRequest::WindowShot` /
  `WindowInput`, appended last.
- **Only registered applications.** `app` (or `app/process`) names one that
  registered through cursio-sdk; the node finds its visible top-level windows
  by the process ids it registered with, the largest first, `title` choosing
  among them. Nothing a peer sends is a handle or a path.
- **Consent: `[apps.<id>] actions`.** Clicking a window does at least what
  calling the application's actions does, so the one answer covers both,
  asked once with buttons on that machine (`apps::actions::allowed`).
- **Windows** (`cursio_window::windows_app`): the still is `PrintWindow` with
  `PW_RENDERFULLCONTENT` — one picture, not a capture session, and the flag
  is what makes WebView2/DirectComposition content print instead of black; a
  minimised window is reported, not returned black. Input is posted
  (`PostMessage`) to the deepest child under the point — where a WebView2 page
  takes clicks — through the replay a transferred window uses, so the
  person's real cursor never moves; text is `WM_CHAR` per UTF-16 unit to the
  focused control, any script. An application reading raw input or
  DirectInput ignores posted input, as with window transfer.
- **Linux, GNOME** (`cursio_window::linux_app` → the Shell extension's
  `appWindows.js`), Wayland and X11 sessions alike. The ScreenCast portal was
  the first candidate and does not fit: it streams only a window the person
  picks in its dialog, and nothing in the stream names the process, so "the
  window of the application registered as pid N" has no portal answer. Inside
  the compositor it is `Meta.Window.get_pid()`. The extension raises and
  focuses the window, takes the Shell's own screenshot of its frame
  (`Shell.Screenshot.screenshot_area`), and drives it with Clutter virtual
  devices — absolute pointer motion and a button for a click (the pointer is
  put back afterwards), keysyms for text (Mutter maps one missing from the
  layout onto a spare key code, so any script types), evdev codes for keys.
  Unlike Windows the window is brought to the front: on Wayland nothing else
  reaches it. The extension answers only a caller whose executable is named
  `cursio`; the node asks only after `[apps.<id>] actions`. A desktop without
  the extension, or with an older one, is told which (`linux_app::explain`).
  A non-GNOME X11 desktop is not built (no XTest/`get_image` path): it answers
  that it needs GNOME with the extension.

## An application's web view

Below the window, an application started with its own `--devtools` flag and
declaring `App::devtools(Devtools::Cdp { port })` can be inspected through
the DevTools protocol (rung 5 in `docs/devtools-design.md`):

- **`apps_devtools { machine, app }`** (destructive) asks the owning machine
  — consent there is `[apps.<id>] devtools`, `deny` by default, granted for
  an hour or until the app restarts — and opens a token-gated listener on
  this machine's loopback, answering its URL for Playwright or the inspector.
  With `launch: true`, when no running copy offers DevTools it starts the
  application there with `--devtools` (ending the copies running without
  them, under that machine's launch consent) and waits up to 30 s.
- **`web_snapshot`**, **`web_query { selector }`**, **`web_screenshot`** and
  **`web_console { wait_ms? }`** read the first page target, and
  **`web_network { wait_ms?, max?, body_for? }`** lists the requests it makes
  while waiting (plus its earlier resources from the performance timeline),
  with one response body, capped, only when `body_for` names it;
  **`web_click { selector }`** and **`web_fill { selector, text }`** change
  it; **`web_eval { expression }`** runs JavaScript in it and is destructive
  with its own title, so a client asks before each call. They reuse the
  listener `apps_devtools` opens (opening one when none is up), and exist
  only in a build with the `web-tools` feature (every desktop build).

## The same surface in cursio-sdk

Applications and xtasks drive the same things programmatically:
`cursio_sdk::dev` (feature `dev`) with `machines()`, `logs(..)`,
`ship(..)`, `launch(..)`, later `phone_*`. It reaches the node through the
`cursio` CLI (`cursio apps ship …`, `cursio logs …`, machine-readable with a
`--json` flag to add) or `cursio mcp` itself as a subprocess speaking JSON —
never by linking `cursio-core`, which is GPL while the SDK is
MIT OR Apache-2.0. A subprocess is a program boundary, not a library one.

## Order

1. Built: the tools in the table; `control::query` shared with the CLI.
2. Built: `phone_screenshot` (image content).
3. Built: `phone_tap/swipe/key/text`.
4. Notifications, messages, clipboard, gallery, ring.
5. `cursio_sdk::dev` over `cursio mcp` / `--json`.

## Trying it

```sh
cursio mcp <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"machines","arguments":{}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"logs","arguments":{"machine":"all","since":"5m","grep":"path"}}}
EOF
claude mcp add cursio -- cursio mcp   # then ask Claude Code: "what did Lightmoon log in the last 10 minutes?"
```
