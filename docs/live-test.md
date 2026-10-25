# Deploying to and measuring the two real machines

Unit tests run against in-memory fakes. What they cannot see is what the user
hits on the real pair — the session loop stalling behind a module, the mount
hanging a caller, input lagging on the Windows side. Two xtask commands cover
that: `deploy` gets the same commit onto both machines, `live-test` turns a
window of both logs into PASS/FAIL numbers.

The Windows peer is reached by ssh into its WSL. Where it is lives in the
checkout's `.env` (ignored by git; copy `.env.example`): `CURSIO_PEER_SSH`,
`CURSIO_PEER_SSH_PORT` (default 22), `CURSIO_PEER_WIN_HOME` and
`CURSIO_PEER_NAME`. The flags `--ssh`, `--ssh-port`, `--win-home` and
`--peer-name` override them for one run. Below, `C:\Users\<you>` is that
`CURSIO_PEER_WIN_HOME`.

## `cargo xtask deploy`

```sh
cargo xtask deploy                         # both halves, restart Windows
cargo xtask deploy --windows-only --no-restart
cargo xtask deploy --linux-only
cargo xtask deploy --windows-only --no-build   # ship the .exe already built
```

- **Linux**: builds `target/release/cursio` (what `cargo xtask run` starts) and
  records its commit beside it in `cursio.commit`. It never stops the running
  daemon — that is the user's foreground terminal — but says when the running
  process is on the previous build and needs a restart.
- **Windows**: cross-builds (into `$CARGO_TARGET_DIR`, default
  `~/.cache/cursio-win-target`), copies the `.exe` to
  `C:\Users\<you>\Cursio\cursio-<commit>[-dirty]-<sha256:8>.exe`, and checks
  sha256 on both ends. The content hash is in the name because a running `.exe`
  is locked: two dirty builds of one commit must not collide, and the running
  one can never be overwritten.
- `deployed.txt` in that folder records every build deploy placed there;
  `current.txt` names the newest and `cursio.cmd` starts it (`cursio.cmd mesh
  ...`). Only the newest `--keep` (3) are kept, and only files listed in the
  manifest **and** matching the naming scheme are deleted — hand-copied
  `cursio-v12.exe` and friends are never touched. A build still running stays.
- **Restart** reads the mesh daemon's arguments from `Win32_Process`,
  writes `deploy-restart.ps1`, and runs it through the scheduled task
  `CursioDeployRestart` with an interactive logon. A process started from ssh
  lives in session 0 with no desktop, where capture and injection do nothing;
  the task runs in the user's session. The script sends Ctrl-C to the old
  daemon's console, waits 8 s, force-stops it if needed, and starts the new
  build with the same arguments. Its log is `deploy-restart.log`. The daemon
  asks for UAC on start, so the prompt appears on the peer's screen.

The last lines printed say which build runs where.

## `cargo xtask live-test`

```sh
cargo xtask live-test                       # passive, 65 s
cargo xtask live-test --yes --seconds 95    # with synthetic pointer motion
cargo xtask live-test --no-files
```

It records where both logs end, drives load for `--seconds`, reads back exactly
the lines written in that window from both machines, and prints a table. It
exits non-zero when any row fails. A row with nothing to judge (no audio
played, no video) is `SKIP`, never `PASS`.

| check | source | limit |
|---|---|---|
| input received->injected p99 | `input latency path="received->injected"` | < 5 ms |
| input captured->queued p99 | `input latency path="captured->queued"` (legacy: `captured->sent`) | < 5 ms |
| process input queue max age | raw capture, captured input, session inbound/outbound queue reports | < 50 ms |
| process queue overloads | explicit queue overload errors | 0 |
| send queue input p99 | `send queue delay` / `input waited long enough` | < 5 ms |
| send queue video p99 (screen) | same lines, `video_p99` | < 200 ms |
| session loop stalls | `session loop turn was slow`, `... blocked the session loop` | none > 50 ms |
| module thread stalls | `module pump was slow`, `module was slow handling a message` | none > 500 ms |
| peer session drops | `peer session ended`, `peer is slow to respond` | 0 |
| audio continuity | `audio from the peer stalled`, `audio health` `stale_dropped` growth, jitter-buffer drops | 0 |
| file sharing health | `file sharing health` `failed`/`served_slow`/`rtt_max_ms`, hung-call warnings (`failed` is the owner's I/O errors and unknown handles; a missing file or a permission is `refused` and not judged) | 0, rtt < 500 ms |
| screen/window errors | encoder failures, blocked surrogate, failed remote-screen view | 0 |
| file roundtrip on peer mount | harness | ok |
| memory/io pressure (full) | `/proc/pressure/{memory,io}` over the window | < 5 % / < 10 % |

Latency is reported every 30 s, so the window must be longer than that.

**Synthetic input** needs `--yes` and the cursor already on the peer (the last
`ownership changed` line here says `active=false`). It creates a uinput mouse
named `xtask live-test pointer` and traces small closed circles at 125 Hz, so
the pointer on the peer wobbles and ends where it started. It enters through the
compositor and the capture portal like a real mouse, so what it measures is what
the user feels. Without `--yes`, or with the cursor here, it injects nothing and
input is judged from whatever the user does.

**The file roundtrip** writes 4 MiB to `C:\Users\<you>\Cursio\live-test\` through
the `(C)` mount under `~/Cursio/<peer>/`, syncs, reads it back, compares the
sha256 on the peer's own disk over ssh, and removes it. The folder is created on
the peer's side, so the mount only looks up names — it is never listed. Each
step runs in a child process with a 20 s deadline: a FUSE call the peer never
answers leaves its caller in uninterruptible sleep, and a stuck child is
reported and abandoned rather than hanging the test.

What `live-test` cannot drive yet: audio and the remote screen are only judged
from traffic the user produces during the window.

`captured->queued` ends at the session outbox admission, not at physical network
transmission. The outbox's age and transport queue delays are measured separately;
their percentiles must not be added to claim an end-to-end percentile. Process
queue peaks and ages describe each ten-second reporting window.
