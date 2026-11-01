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

## Android

On a phone where the Cursio app runs, the same code reaches its node on the
abstract socket `@cursio-apps` — no Java or Kotlin on your side. The first
connection asks the phone's person to let your app talk to Cursio under the
id it registered; until then `start` keeps retrying in the background.

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

## Files between copies

Copies of an application on different machines fetch files from each other
by content hash — the hash a modpack manifest already carries:

```rust
use cursio_sdk::{App, BlobAlgo, BlobEntry, BlobHash};

let cursio = App::new("my-app")
    .on_blob(|hash| library.path_of(hash)) // the file holding it, or None
    .start()?;
let wanted = vec![BlobEntry {
    path: "mods/sodium.jar".into(),
    hash: BlobHash::new(BlobAlgo::Sha1, "…40 hex digits…")?,
    size: 1_234_567,
}];
let report = cursio.fetch(&wanted, &cache_dir, |p| println!("{}/{}", p.received, p.size))?;
// report.fetched — in cache_dir under BlobHash::file_name()
// report.not_found — no copy nearby has it: get it from your CDN
```

Bytes count only once they hash right; an interrupted fetch resumes. The node
reads only files `on_blob` names, and gives them only to machines whose
person lets the asker drive the application.

Names of actions, topics and the process are lowercase letters, digits and
`-` (`game-log`, not `game_log`). A debug build panics at a bad one where it
is written; a release build leaves that one out, registers everything else,
and lists what it skipped in `Cursio::rejected()` — one mistyped name never
takes the whole application out of the mesh.

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

## DevTools, behind the application's own flag

```rust
let mut app = App::new("my-app");
if devtools_flag {
    app = app.devtools(Devtools::Cdp { port });  // the web view's loopback CDP port
}
```

The node opens the port to another machine (`cursio devtools Laptop my-app`)
only after checking that one of the application's processes listens on it on
loopback, and only for as long as that machine's person allows — an hour, or
until the application restarts; `[apps.my-app] devtools` is `deny` until they
say otherwise. It is a separate answer from actions because DevTools runs any
code inside the application, so a release build should not declare it.

`MIT OR Apache-2.0`. See the [repository](https://github.com/THEROER/cursio-sdk).
