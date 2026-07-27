//! Print this machine's audio output devices as Cursio sees them for mirroring.
//!
//! ```sh
//! cargo run -p cursio-audio --example list_devices
//! ```

fn main() {
    match cursio_audio::list_output_devices() {
        Ok(devs) => {
            println!("{} output device(s):", devs.len());
            for d in devs {
                println!("  id={:<45} description={}", d.id, d.description);
            }
        }
        Err(e) => {
            eprintln!("failed to list devices: {e}");
            std::process::exit(1);
        }
    }

    println!();
    match cursio_audio::list_app_streams() {
        Ok(apps) => {
            println!("{} app stream(s) playing:", apps.len());
            for a in apps {
                println!("  id={:<12} app={}", a.id, a.app_name);
            }
        }
        Err(e) => eprintln!("failed to list app streams: {e}"),
    }
}
