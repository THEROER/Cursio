//! Print this desktop's screens as the layout sees them: where each stands in
//! capture's pixels, and its panel's size.
//!
//! ```text
//! cargo run -p cursio-input --example screens_probe
//! ```

fn main() {
    match cursio_input::screens::screens() {
        Ok(screens) => {
            for s in screens {
                let size = s
                    .size_mm
                    .map(|(w, h)| format!("{w}x{h} mm"))
                    .unwrap_or_else(|| "size unknown".to_string());
                println!(
                    "{:<40} {:>5},{:<5} {:>5}x{:<5} {size}  [{}]",
                    s.name, s.x, s.y, s.width, s.height, s.id
                );
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
