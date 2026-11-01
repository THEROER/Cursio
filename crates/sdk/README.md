# cursio-sdk

The client an application links to put itself on a Cursio mesh: the Cursio
node on its machine carries its logs to every other machine, tells it when a
new build of it is installed, and hears how it is doing.

```rust
let cursio = cursio_sdk::App::new("my-app")
    .version(env!("CARGO_PKG_VERSION"))
    .logs("/home/me/.local/share/my-app/logs", "my-app.log")
    .on_update(|build| println!("{} installed in {}", build.version, build.dir.display()))
    .start()?;
```

Without a running node the application works exactly as before: `start`
fails only for an invalid id or log path, and the connection is made, and made
again, in the background.

## Actions

A running process can let another machine of the mesh — a person, or an AI
agent through `cursio mcp` — ask it to do something, by name:

```rust
use cursio_sdk::{json, App};

let cursio = App::new("my-app")
    .action("open", "Open a document", json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"]
    }), |args| {
        let path = args["path"].as_str().ok_or("path is required")?;
        open(path).map(|_| json!({ "opened": path })).map_err(|e| e.to_string())
    })
    .start()?;
```

```sh
cursio apps actions Laptop my-app
cursio apps call Laptop my-app open '{"path": "notes.txt"}'
```

Handlers run off the connection's thread; a panic answers that it happened
and later calls go on. The machine's person allows calls to an application
once, with a button.

## Messages between copies

Copies of one application on different machines talk to each other over the
mesh's encryption, with no server of their own:

```rust
let cursio = App::new("my-app")
    .on_message("notes", |m| println!("{} sent {} bytes", m.from_machine, m.payload.len()))
    .start()?;
cursio.send("Laptop", "notes", b"changed: notes.txt")?;  // blocks until taken
cursio.send_all("notes", b"changed: notes.txt")?;        // every copy listening
let where_it_runs = cursio.peers()?;
```

At most 512 KiB a message, delivered at most once to the copies running now;
nothing is kept for later. A failure says which case it was —
`Error::Offline`, `Error::CopyNotRunning`, `Error::NotAllowed` — so the
application decides whether trying again means anything. A machine takes
messages only from a machine its person lets drive the application, the same
answer as for actions.

`MIT OR Apache-2.0`. See the [repository](https://github.com/THEROER/cursio-sdk).
