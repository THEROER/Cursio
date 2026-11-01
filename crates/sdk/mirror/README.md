# cursio-sdk

Put an application on a [Cursio](https://github.com/THEROER) mesh — the
developer's own machines and phones, paired once, sharing one encrypted
channel over the LAN and Bluetooth.

The application never joins the mesh itself. It tells the Cursio node on its
own machine who it is, where it logs and how it is doing, and the node answers
for it to every other machine:

```rust
let cursio = cursio_sdk::App::new("my-app")
    .version(env!("CARGO_PKG_VERSION"))
    .logs("/home/me/.local/share/my-app/logs", "my-app.log")
    .on_update(|build| println!("{} is installed in {}", build.version, build.dir.display()))
    .start()?;
```

From any machine of the mesh:

```sh
cursio logs Laptop --app my-app -f                 # its log, live, no SSH
cursio apps ship my-app --to Laptop windows-x86_64=target/my-app.exe   # build there, started
cursio apps call Laptop my-app open '{"path": "notes.txt"}'           # an action it declared
```

An application declares actions with `App::action(name, description,
schema, handler)`; the running process does them when another machine — or
an AI agent through `cursio mcp` — calls, once that machine's person allows
calls to it.

A machine takes an application's builds only after its person trusts the
builder for that application, once, with a button.

## Cursio being absent is ordinary

`App::start` fails only for a mistake in the program (an invalid id or log
path), never because no node answered. The connection lives on its own thread
and reconnects with a backoff; calls made meanwhile return
`Error::NotRunning` and are remembered.

## Crates

| Crate | What |
|---|---|
| [`cursio-sdk`](crates/sdk) | the client an application links |
| [`cursio-app-protocol`](crates/app-protocol) | what an application and its node say to each other, pinned byte for byte |

## License

`MIT OR Apache-2.0`, at your option. Neither crate contains or links Cursio's
own code; they reach the node only through its local socket, so linking them
puts no condition of Cursio's license on an application.

This repository is a mirror: the crates are developed inside Cursio, where the
node's side of the protocol changes in the same commit, and published from
here by `cargo xtask sdk-mirror`.
