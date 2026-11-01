# cursio-app-protocol

What an application and the Cursio node on its machine say to each other: one
JSON object per line over the node's private local socket (a Unix socket, or a
named pipe on Windows), versioned, with every message's bytes pinned by tests
so that an application built against an older version keeps working when the
node updates.

Most applications want [`cursio-sdk`](https://crates.io/crates/cursio-sdk),
which speaks it for them.

`MIT OR Apache-2.0`.
