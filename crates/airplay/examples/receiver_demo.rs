use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use clap::{Parser, ValueEnum};
use cursio_airplay::{Receiver, ReceiverConfig, Resolution, VideoProfile};
use cursio_transport::discovery::DiscoveryConfig;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(about = "Advertise and probe the Cursio AirPlay receiver foundation")]
struct Args {
    #[arg(long, default_value = "Cursio AirPlay")]
    name: String,
    #[arg(long, default_value_t = 7000)]
    port: u16,
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    bind: IpAddr,
    #[arg(long)]
    interface: Option<IpAddr>,
    #[arg(long, value_enum, default_value_t = ResolutionArg::FullHd)]
    res: ResolutionArg,
    #[arg(long, default_value_t = 60)]
    fps: u16,
    #[arg(long, default_value = "c07510a0-0000-4000-8000-000000000001")]
    identity: Uuid,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ResolutionArg {
    #[value(name = "1080p")]
    FullHd,
    #[value(name = "2k")]
    Qhd,
}

impl From<ResolutionArg> for Resolution {
    fn from(value: ResolutionArg) -> Self {
        match value {
            ResolutionArg::FullHd => Self::FullHd,
            ResolutionArg::Qhd => Self::Qhd,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    let profile = VideoProfile::new(args.res.into(), args.fps)?;
    let hardware = cursio_airplay::hardware::HardwareInfo::detect();
    for gpu in &hardware.gpus {
        let vram = gpu
            .dedicated_vram_bytes
            .map(|bytes| format!("{:.1} GiB", bytes as f64 / 1024_f64.powi(3)))
            .unwrap_or_else(|| "shared/unknown VRAM".to_string());
        println!("GPU: {} ({vram})", gpu.name);
    }
    for warning in hardware.profile_warnings(profile) {
        eprintln!("warning: {warning}");
    }
    let config = ReceiverConfig::new(
        args.name,
        args.identity,
        SocketAddr::new(args.bind, args.port),
        profile,
    )?;
    let discovery = args
        .interface
        .map_or_else(DiscoveryConfig::default, DiscoveryConfig::pinned_to);
    let receiver = Receiver::bind(config, discovery).await?;
    let address = receiver.local_addr()?;

    println!("AirPlay demo is listening on {address}");
    println!("Open Control Center → Screen Mirroring and select “Cursio AirPlay”.");
    println!("Discovery and /info work; pairing and video SETUP currently fail closed.");

    receiver.serve().await?;
    Ok(())
}
