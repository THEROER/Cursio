//! Drives this crate by hand on real machines, since what it does needs a
//! Wi-Fi card, NetworkManager or WinRT and a second machine nearby.
//!
//! ```text
//! direct_link_probe ability
//! direct_link_probe host <seconds>               # Windows: prints the credentials
//! direct_link_probe join <ssid> <pass> <seconds>  # Linux: joins, prints both addresses
//! ```

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let secs = |i: usize| -> u64 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(30) };
    match args.get(1).map(String::as_str) {
        Some("ability") => println!("{:?}", cursio_wifidirect::ability()),
        Some("host") => match cursio_wifidirect::host() {
            Ok(group) => {
                println!("{:?}", cursio_wifidirect::ability());
                println!("ssid {} pass {}", group.ssid(), group.passphrase());
                std::thread::sleep(Duration::from_secs(secs(2)));
                println!("still up: {}", group.is_up());
            }
            Err(e) => println!("host: {e}"),
        },
        Some("join") if args.len() >= 4 => {
            println!("{:?}", cursio_wifidirect::ability());
            let t = std::time::Instant::now();
            match cursio_wifidirect::join(&args[2], &args[3]) {
                Ok(joined) => {
                    println!(
                        "joined in {} ms: here {}, host {}",
                        t.elapsed().as_millis(),
                        joined.local,
                        joined.host
                    );
                    std::thread::sleep(Duration::from_secs(secs(4)));
                    println!("still up: {}", joined.is_up());
                }
                Err(e) => println!("join: {e}"),
            }
        }
        _ => eprintln!("usage: direct_link_probe ability | host <s> | join <ssid> <pass> <s>"),
    }
}
