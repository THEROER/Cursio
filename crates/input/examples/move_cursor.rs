//! Manual smoke test for the uinput injector: moves the real cursor in a
//! visible pattern so you can watch it work.
//!
//! Run on a Linux machine with write access to `/dev/uinput`:
//!
//! ```sh
//! cargo run -p cursio-input --example move_cursor
//! ```
//!
//! The cursor should trace a square, then left-click. If nothing moves, the
//! compositor may not have enumerated the virtual device yet (wait a moment) or
//! `/dev/uinput` isn't writable.

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cursio_input::{InputInjector, UinputInjector};
    use cursio_protocol::{InputEvent, MouseButton};
    use std::{thread::sleep, time::Duration};

    tracing_subscriber::fmt().with_env_filter("info").init();

    let mut injector = UinputInjector::new()?;
    println!(
        "virtual pointer up ({}). Giving the compositor 2s to notice it...",
        injector.backend()
    );
    // The compositor needs a moment to enumerate a freshly created uinput device.
    sleep(Duration::from_secs(2));

    // Trace a square: 100px each side, in ~20px steps so it's visible.
    let step = 20;
    let side = 100;
    let legs = [
        (step, 0),  // right
        (0, step),  // down
        (-step, 0), // left
        (0, -step), // up
    ];
    println!("tracing a square with the cursor...");
    for (dx, dy) in legs {
        for _ in 0..(side / step) {
            injector.inject(InputEvent::PointerMotion { dx, dy })?;
            sleep(Duration::from_millis(40));
        }
    }

    println!("left click...");
    injector.inject(InputEvent::PointerButton {
        button: MouseButton::Left,
        pressed: true,
    })?;
    sleep(Duration::from_millis(50));
    injector.inject(InputEvent::PointerButton {
        button: MouseButton::Left,
        pressed: false,
    })?;

    // Toggle inactive and confirm motion is now suppressed (cursor won't move).
    println!("going inactive — the next motion batch should NOT move the cursor...");
    injector.set_active(false);
    for _ in 0..10 {
        injector.inject(InputEvent::PointerMotion { dx: 30, dy: 0 })?;
        sleep(Duration::from_millis(40));
    }
    println!("done. If the cursor stayed put during the last step, active/inactive works.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("this example only runs on Linux (uinput)");
}
