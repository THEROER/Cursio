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

`MIT OR Apache-2.0`. See the [repository](https://github.com/THEROER/cursio-sdk).
