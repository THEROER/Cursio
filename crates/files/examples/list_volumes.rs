//! What this machine offers the mesh, including removable media.
//!
//! Detection reads the real mount table and the kernel's `removable`
//! attribute, which no unit test can: a fixture proves the parsing, only a real
//! machine proves the assumption that fixed disks under `/mnt` are not mistaken
//! for sticks. Run it with and without a USB stick plugged in.
//!
//! `cargo run -p cursio-files --example list_volumes`

fn main() {
    let server = cursio_files::FileServer::detect();
    let volumes = server.volumes();

    println!("{:<28} {:<12} {:>9} FREE / TOTAL", "ID", "LABEL", "KIND");
    for v in &volumes {
        let kind = match v.removable {
            true => "removable",
            false => "fixed",
        };
        println!(
            "{:<28} {:<12} {:>9} {} / {}",
            v.id,
            v.label,
            kind,
            human(v.free_bytes),
            human(v.total_bytes)
        );
    }
    println!(
        "\n{} volumes, {} removable",
        volumes.len(),
        volumes.iter().filter(|v| v.removable).count()
    );
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}
