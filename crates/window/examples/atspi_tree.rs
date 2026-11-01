//! Print the interface tree of a running application's window, read through
//! AT-SPI — what `cursio window <machine> <app> tree` answers, without a peer
//! or an SDK registration. Read only: nothing is pressed.
//!
//! cargo run -p cursio-window --example atspi_tree -- <pid> [title] [depth] [nodes]

#[cfg(target_os = "linux")]
fn main() {
    use cursio_window::{linux_atspi, ui_tree};
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(pid) = args.first().and_then(|p| p.parse::<u32>().ok()) else {
        eprintln!("usage: atspi_tree <pid> [title] [depth] [nodes]");
        std::process::exit(2);
    };
    let title = args.get(1).filter(|t| !t.is_empty()).cloned();
    let mut limits = ui_tree::Limits::default();
    if let Some(depth) = args.get(2).and_then(|d| d.parse().ok()) {
        limits.depth = depth;
    }
    if let Some(nodes) = args.get(3).and_then(|n| n.parse().ok()) {
        limits.nodes = nodes;
    }
    let started = std::time::Instant::now();
    match linux_atspi::tree(vec![pid], title, limits) {
        Ok(snapshot) => {
            print!("{}", ui_tree::render(&snapshot.root));
            eprintln!(
                "{:?}: {} elements{} in {:?}",
                snapshot.title,
                snapshot.nodes,
                if snapshot.truncated {
                    ", truncated"
                } else {
                    ""
                },
                started.elapsed()
            );
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("atspi_tree reads AT-SPI, which is Linux's accessibility bus");
    std::process::exit(1);
}
