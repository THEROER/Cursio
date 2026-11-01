//! Registers `sdk-demo` with the node on this machine and writes a log line a
//! second, so `cursio logs <this machine> --app sdk-demo -f` can be tried
//! from any machine of the mesh. Needs a node built with the apps socket.

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cursio_sdk::{App, HealthState};

fn main() {
    let dir = std::env::temp_dir().join("cursio-sdk-demo");
    std::fs::create_dir_all(&dir).expect("a temp directory");
    let cursio = App::new("sdk-demo")
        .version(env!("CARGO_PKG_VERSION"))
        .logs(&dir, "sdk-demo.log")
        .start()
        .expect("a valid id");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("sdk-demo.log"))
        .expect("the demo log");
    let mut tick = 0u64;
    loop {
        tick += 1;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let stamp = rfc3339(now.as_secs());
        writeln!(log, "{stamp}  INFO sdk-demo: tick {tick}").expect("the demo log");
        let state = match cursio.machine() {
            Some(machine) => format!("connected to the node on {machine}"),
            None => "no node yet".into(),
        };
        println!("tick {tick}: {state}");
        let _ = cursio.set_health(HealthState::Ok, Some(&format!("tick {tick}")));
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// `secs` since the epoch as the UTC timestamp `tracing` writes, which the
/// node's log reader orders lines by.
fn rfc3339(secs: u64) -> String {
    let days = secs / 86_400;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.000000Z")
}
