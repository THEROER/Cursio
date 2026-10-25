# Contributing

Bug reports, fixes and new platform backends are welcome. Most of Cursio's
behaviour exists because something simpler did not survive a real machine, so
please read the design document of the subsystem you touch before changing it:
`docs/*.md` are the source of truth, and a change that diverges from one fixes
the document or the code in the same commit.

## Reporting a bug

Say which machines and operating systems were on each end, what you ran
(`cursio config` prints the effective settings), and attach the log from
**both** ends — `cargo xtask logs` prints where it is. Most Cursio bugs are a
disagreement between two nodes, and one side's log rarely shows it.

Security problems go through [SECURITY.md](SECURITY.md) instead.

## Building

Always scope cargo to a package: `--workspace` fails on macOS, because
`apps/clipd` needs GTK4. `cargo xtask` sets the environment each build needs.

```sh
cargo xtask build                       # release build for this host
cargo build -p cursio-core              # the `cursio` CLI
cargo test -p cursio-core peer_timeout  # tests whose name contains this
cargo clippy -p <pkg> --all-targets     # must be clean
cargo fmt -p <pkg>
```

Native Linux builds need `OPUS_TARGET=ubuntu-24.04_x86_64` (`cargo xtask` sets
it), Windows cross-builds need `GSTREAMER_WINDOWS_SYSROOT`. The README's
"Building & installing" section has the rest.

## Code

- **Comments explain why**, and only where the reason is not recoverable from
  the code. Each module opens with a `//!` block arguing its design — what was
  tried, what the platform refuses to do, what breaks if it is simplified.
  Narration (`// increment counter`) and notes about the edit itself do not
  belong.
- **Tests** live in `#[cfg(test)] mod tests` at the bottom of the file they
  cover and are named as behavioural sentences
  (`rejects_control_characters_in_header_values`). What needs a compositor, a
  GPU or a peer is tested through the in-memory fakes
  (`transport::test_util::MemTransport`, `window::window_test_support`); the
  crates' `examples/` probe real hardware and stay examples.
- **Errors**: one `thiserror` enum per library crate, `anyhow` in binaries.
  Messages are lowercase, name the cause, and say what to install when that is
  the fix. Never swallow one; degrade with `tracing::warn!` and a reason.
- **Platform code** is gated with `#[cfg(target_os = ...)]` and always has a
  fallback arm returning an explicit error.
- **A feature is a module** (`core/src/module.rs`), not a branch in the CLI.
- **Wire changes** bump `PROTOCOL_VERSION`; the update lane
  (`protocol/src/update.rs`) is never changed.
- Dependencies are declared once in the root `[workspace.dependencies]`.
- No `TODO`/`FIXME`, placeholders or commented-out code in a commit.

## Commits

Conventional Commits, subject line only:

```
fix(airplay): keep cipher aligned for idr frames
```

The scope is the subsystem (`input`, `audio`, `window`, `files`, `mesh`,
`transport`, …), and the summary says what changes in behaviour, not which file
was edited. One commit per issue, each one compiling.

## License

Cursio is GPL-3.0-or-later ([LICENSE](LICENSE)). By submitting a contribution
you agree that it is licensed under the same terms. Code taken from elsewhere
must carry a GPL-3.0-compatible license, keep its notices, and be listed in
[THIRD-PARTY.md](THIRD-PARTY.md).
