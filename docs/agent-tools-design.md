# The mesh as tools for an AI agent (`cursio mcp`)

Status: **first slice built** (machines, logs, report, path, apps status /
publish / launch / ship, restart, apply-update). The phone's screen and touch
are designed below and not built.

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

## The phone: screen and touch (designed, not built)

What exists: `cursio screen <phone>` / `Command::PhoneScreen` asks the phone
to share its screen (it asks its person — Android's own consent, once per
process); the phone's MediaCodec H.264 arrives as an ordinary transferred
window; the viewer's pointer comes back as `WindowInput` and
`phone_gesture.rs` turns it into touch, which `ScreenTouch.kt` (an
accessibility service the person turns on once) dispatches.

What an agent needs on top:

- **`phone_screenshot { phone }` → an MCP `image` content (PNG).** The node
  that holds the phone's screen session keeps the last decoded frame of that
  window (the surrogate's `Framebuffer` already holds it); a new control
  command `ScreenFrame { peer }` returns it PNG-encoded. Without a session
  open, the tool opens one (`PhoneScreen { show: true }` with no window — a
  hidden surrogate, as remote-screen views already are on Windows) and
  answers "waiting for the phone's person to allow sharing" until they do.
- **`phone_tap { phone, x, y }`, `phone_swipe { phone, from, to, ms }`,
  `phone_key { phone, key: back | home | recents }`, `phone_type { phone,
  text }`.** Coordinates normalised 0..1, as `phone_gesture::Point` is.
  A new control command `PhoneInput { peer, gesture }` builds the
  `WindowInput` sequence `phone_gesture` already understands and sends it
  into the open screen session; text goes as key events, or through the
  clipboard plus a paste where the field takes it. All destructive-marked:
  they act in the person's phone UI.
- **`phone_logs { phone, app? }`.** The phone's own Cursio log already reads
  through `logs` (its node keeps a file). Android gives an app no other app's
  logcat (`READ_LOGS` is a system permission), so another application's log
  on a phone reads through cursio-sdk on Android (step 6 of
  `docs/app-sdk-design.md`) — not through logcat.
- Cheap next: `notifications` (the notify module's recent list), `messages`
  (`ListConversations`/`ListMessages`, reads; `SendMessage` destructive),
  `clipboard_get/set`, `gallery_list/fetch`, `ring`.

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
2. `ScreenFrame` + `phone_screenshot` (image content), hidden screen session.
3. `PhoneInput` + `phone_tap/swipe/key/type`.
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
