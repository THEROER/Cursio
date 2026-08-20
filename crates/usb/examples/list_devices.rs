//! What this machine would offer the mesh.
//!
//! Enumeration reads the real `/sys/bus/usb/devices`, which no unit test can:
//! the fixtures in `linux.rs` assert the parsing, but only actual hardware
//! shows whether the tree looks the way the parser assumes. Run it after
//! touching enumeration, and after plugging in something unusual.
//!
//! `cargo run -p cursio-usb --example list_devices`

fn main() {
    #[cfg(target_os = "linux")]
    {
        let usb = cursio_usb::LinuxUsb::new();
        let devices = match usb.devices() {
            Ok(devices) => devices,
            Err(e) => {
                eprintln!("cannot enumerate: {e}");
                std::process::exit(1);
            }
        };

        println!(
            "{:<8} {:<10} {:<6} {:<9} NAME",
            "BUSID", "ID", "CLASS", "SHARE"
        );
        for d in &devices {
            let name = match (&d.manufacturer, &d.product_name) {
                (Some(m), Some(p)) => format!("{m} {p}"),
                (None, Some(p)) => p.clone(),
                (Some(m), None) => m.clone(),
                (None, None) => "-".to_string(),
            };
            let share = match (d.is_shareable(), d.has_isochronous) {
                (false, _) => "no (hub)",
                (true, true) => "degraded",
                (true, false) => "yes",
            };
            println!(
                "{:<8} {:04x}:{:04x} 0x{:02x}   {:<9} {}",
                d.busid, d.vendor, d.product, d.class, share, name
            );
        }
        println!(
            "\n{} devices, {} offerable",
            devices.len(),
            devices.iter().filter(|d| d.is_shareable()).count()
        );
    }

    #[cfg(not(target_os = "linux"))]
    eprintln!("enumeration is implemented for linux only");
}
