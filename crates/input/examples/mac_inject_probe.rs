//! Post real input into this Mac and watch it happen.
//!
//! Injection cannot be unit-tested: `CGEventPost` reports nothing, a process
//! without the Accessibility permission gets exactly the same silence as one
//! that worked, and the only observer that matters is the window server. So
//! this drives the injector against the live machine and says what it did,
//! which is how the permission, the keycode table and the modifier mask are
//! actually checked.
//!
//! Run it with a text field focused — the keystrokes land wherever the cursor
//! is:
//!
//! ```sh
//! cargo run -p cursio-input --example mac_inject_probe
//! ```
//!
//! The pointer should trace a square, the page under it should scroll, `hi`
//! should appear, and Ctrl+A should reach the Mac as Control+A. It selects all
//! only under the `swapped` modifier map, which this probe does not set — the
//! default is by position, so Control stays Control.

#[cfg(target_os = "macos")]
fn main() {
    use std::thread::sleep;
    use std::time::Duration;

    use cursio_input::{InputInjector, MacInjector};
    use cursio_protocol::InputEvent;

    // evdev codes for the keys used below.
    const KEY_A: u16 = 30;
    const KEY_I: u16 = 23;
    const KEY_H: u16 = 35;
    const KEY_LEFTCTRL: u16 = 29;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "debug".into()),
        )
        .init();

    let mut injector = match MacInjector::new() {
        Ok(injector) => injector,
        Err(error) => {
            eprintln!("cannot inject: {error}");
            std::process::exit(1);
        }
    };
    println!("backend: {}", injector.backend());

    println!("moving the pointer in a square");
    for (dx, dy) in [(5, 0), (0, 5), (-5, 0), (0, -5)] {
        for _ in 0..24 {
            injector
                .inject(InputEvent::PointerMotion { dx, dy })
                .expect("motion");
            sleep(Duration::from_millis(8));
        }
    }

    println!("scrolling down then up");
    for dy in [
        -3 * cursio_protocol::SCROLL_NOTCH,
        3 * cursio_protocol::SCROLL_NOTCH,
    ] {
        injector
            .inject(InputEvent::PointerScroll { dx: 0, dy })
            .expect("scroll");
        sleep(Duration::from_millis(120));
    }

    println!("typing 'hi' — focus a text field to see it");
    sleep(Duration::from_secs(2));
    for key in [KEY_H, KEY_I] {
        injector
            .inject(InputEvent::Key { key, pressed: true })
            .expect("key down");
        injector
            .inject(InputEvent::Key {
                key,
                pressed: false,
            })
            .expect("key up");
        sleep(Duration::from_millis(60));
    }

    println!("sending Ctrl+A, which should arrive as Command+A and select all");
    for (key, pressed) in [
        (KEY_LEFTCTRL, true),
        (KEY_A, true),
        (KEY_A, false),
        (KEY_LEFTCTRL, false),
    ] {
        injector
            .inject(InputEvent::Key { key, pressed })
            .expect("modified key");
        sleep(Duration::from_millis(30));
    }

    // Whatever happened above, nothing may be left held: a stuck Command turns
    // every later keystroke into a shortcut.
    injector.release_all().expect("release");
    println!("done");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this probe injects through CoreGraphics and only runs on macOS");
}
