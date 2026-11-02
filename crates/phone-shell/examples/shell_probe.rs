//! Pair with a phone's wireless debugging and run a command as its `shell`
//! user — whether a phone of this make lets Cursio do what adb does.
//!
//! ```text
//! cargo run -p cursio-phone-shell --example shell_probe -- KEYDIR pair HOST:PORT CODE
//! cargo run -p cursio-phone-shell --example shell_probe -- KEYDIR exec HOST:PORT getprop ro.build.version.release
//! ```
//!
//! The pairing address and code are on the phone's "Pair device with pairing
//! code" screen; the address `exec` takes is the other one, on the "Wireless
//! debugging" screen itself.

use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: shell_probe KEYDIR pair HOST:PORT CODE | KEYDIR exec HOST:PORT COMMAND...";
    let (Some(dir), Some(what), Some(addr)) = (args.first(), args.get(1), args.get(2)) else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    let key = match cursio_phone_shell::key(&PathBuf::from(dir), "cursio@probe") {
        Ok(key) => key,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let result = match what.as_str() {
        "pair" => match args.get(3) {
            Some(code) => cursio_phone_shell::pair(addr, code, &key)
                .await
                .map(|guid| format!("paired; the phone is {guid}\n").into_bytes()),
            None => {
                eprintln!("{usage}");
                std::process::exit(2);
            }
        },
        "exec" => {
            let command: Vec<&str> = args[3..].iter().map(String::as_str).collect();
            cursio_phone_shell::exec(addr, key, &command).await
        }
        _ => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    };
    match result {
        Ok(out) => print!("{}", String::from_utf8_lossy(&out)),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
