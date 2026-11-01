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
cursio apps send Laptop my-app notes 'changed'                        # a message to its copies there
```

Copies of the application on different machines talk to each other with
`App::on_message(topic, handler)` and `Cursio::send` / `send_all` /
`peers` — over the mesh's encryption, at most 512 KiB a message, delivered at
most once to the copies running now.

An application declares actions with `App::action(name, description,
schema, handler)`; the running process does them when another machine — or
an AI agent through `cursio mcp` — calls, once that machine's person allows
calls to it.

Copies on different machines fetch files from each other by content hash
(`App::on_blob` names the file for a hash, `Cursio::fetch` gets a list of
them): verified before they count, resumed after a dropped link, and what no
copy nearby holds comes back as not found, for the application to get from
wherever else it keeps it.

A machine takes an application's builds only after its person trusts the
builder for that application, once, with a button.

An application with a Chromium web view started with its own `--devtools`
flag declares its loopback DevTools port with
`App::devtools(Devtools::Cdp { port })`. The node checks that the port is
this process's and opens it to another machine (`cursio devtools Laptop
my-app`) only for a limited time its person allows, separately from
everything above. Declare it only behind that flag: it lets the inspecting
machine run any code inside the application.

## Android

The same code runs inside an Android app on a phone where the Cursio app
runs: the SDK reaches the node on the abstract socket `@cursio-apps`, with no
Java or Kotlin on your side. The first time your app connects, the phone's
person is asked to let it talk to Cursio, under the application id it
registered; until then `start` keeps retrying in the background, and no
other app can later register under that id.

An Android app's log files are private to it, so the node cannot read them.
Send the lines instead — `cursio.log(level, target, message)`, or
`cursio_sdk::tracing_layer(&cursio)` with the `tracing` feature — and the
node keeps the file that `cursio logs <machine> --app <id>` reads. Lines are
queued and batched, never blocking the app; the oldest are dropped and
counted when it logs faster than they leave.

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
