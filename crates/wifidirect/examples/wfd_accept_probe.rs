//! Settles whether a Windows machine accepts a Wi-Fi Direct link from a peer
//! while it stays connected to its access point, and how fast that link is.
//!
//! Run it on Windows, then connect from the other machine (NetworkManager's
//! `wifi-p2p` connection to this machine's P2P address). It advertises, accepts
//! the first request with no dialog, prints both ends' addresses, then pulls
//! from `<peer>:5270` and pushes to `<peer>:5271` for eight seconds each — the
//! probe dials out because an inbound port would wait on a firewall prompt.
//! The pairing Windows records for the peer is removed on the way out.

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    imp::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "wfd_accept_probe runs on Windows; build it with cargo xwin for x86_64-pc-windows-msvc"
    );
    std::process::exit(2);
}

#[cfg(windows)]
mod imp {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use windows::core::{Interface, Ref, Result, HSTRING};
    use windows::Devices::Enumeration::{
        DeviceInformation, DeviceInformationCustomPairing, DevicePairingKinds,
        DevicePairingProtectionLevel, DevicePairingRequestedEventArgs, DeviceWatcher,
        IDevicePairingSettings,
    };
    use windows::Devices::WiFiDirect::{
        WiFiDirectAdvertisementListenStateDiscoverability, WiFiDirectAdvertisementPublisher,
        WiFiDirectAdvertisementPublisherStatusChangedEventArgs, WiFiDirectConfigurationMethod,
        WiFiDirectConnectionListener, WiFiDirectConnectionParameters,
        WiFiDirectConnectionRequestedEventArgs, WiFiDirectDevice, WiFiDirectDeviceSelectorType,
    };
    use windows::Foundation::TypedEventHandler;
    use windows::Security::Credentials::PasswordCredential;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

    pub fn run() -> Result<()> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        let wait = std::env::args()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(120u64);
        let argv: Vec<String> = std::env::args().collect();
        let after = |key: &str, n: usize| {
            argv.iter()
                .position(|a| a == key)
                .and_then(|i| argv.get(i + n).cloned())
        };
        if let Some(ip) = after("bench", 1) {
            let ip = HSTRING::from(ip);
            pull(&ip);
            push(&ip);
            return Ok(());
        }
        if let (Some(ssid), Some(pass)) = (after("legacy", 1), after("legacy", 2)) {
            return legacy(&ssid, &pass, wait);
        }

        let autonomous = std::env::args().any(|a| a == "ago");
        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let adv = publisher.Advertisement()?;
        adv.SetListenStateDiscoverability(
            WiFiDirectAdvertisementListenStateDiscoverability::Normal,
        )?;
        adv.SetIsAutonomousGroupOwnerEnabled(autonomous)?;
        // Empty by default, and then the probe response advertises no WPS
        // method at all: a push-button request from wpa_supplicant is refused
        // as invalid parameters before the app ever hears of it.
        adv.SupportedConfigurationMethods()?
            .Append(WiFiDirectConfigurationMethod::PushButton)?;
        publisher.Start()?;
        println!("autonomous group owner: {autonomous}");
        println!("advertising: status {:?}", publisher.Status()?);

        let (tx, rx) = mpsc::channel::<DeviceInformation>();
        let listener = WiFiDirectConnectionListener::new()?;
        listener.ConnectionRequested(&TypedEventHandler::new(
            move |_: Ref<WiFiDirectConnectionListener>,
                  args: Ref<WiFiDirectConnectionRequestedEventArgs>| {
                if let Some(args) = args.as_ref() {
                    let info = args.GetConnectionRequest()?.DeviceInformation()?;
                    let _ = tx.send(info);
                }
                Ok(())
            },
        ))?;

        let args: Vec<String> = std::env::args().collect();
        let dial = args
            .iter()
            .position(|a| a == "dial")
            .and_then(|i| args.get(i + 1))
            .map(|s| s.to_lowercase());
        let info = if let Some(name) = dial {
            let Some(info) = find(&name, wait)? else {
                println!("no device named like {name:?} turned up");
                publisher.Stop()?;
                return Ok(());
            };
            println!("dialling {} ({})", info.Name()?, info.Id()?);
            info
        } else {
            println!("waiting {wait} s for a connection request");
            let Ok(info) = rx.recv_timeout(Duration::from_secs(wait)) else {
                println!("no request arrived");
                publisher.Stop()?;
                return Ok(());
            };
            println!("request from {} ({})", info.Name()?, info.Id()?);
            info
        };

        let t = Instant::now();
        let custom: DeviceInformationCustomPairing = info.Pairing()?.Custom()?;
        custom.PairingRequested(&TypedEventHandler::new(
            |_: Ref<DeviceInformationCustomPairing>, args: Ref<DevicePairingRequestedEventArgs>| {
                if let Some(args) = args.as_ref() {
                    println!("pairing kind {:?}, accepting", args.PairingKind()?);
                    args.Accept()?;
                }
                Ok(())
            },
        ))?;
        let params = WiFiDirectConnectionParameters::new()?;
        params.SetGroupOwnerIntent(15)?;
        let pairing = custom
            .PairWithProtectionLevelAndSettingsAsync(
                DevicePairingKinds::ConfirmOnly
                    | DevicePairingKinds::DisplayPin
                    | DevicePairingKinds::ProvidePin,
                DevicePairingProtectionLevel::Default,
                &params.cast::<IDevicePairingSettings>()?,
            )?
            .get()?;
        println!(
            "pairing: {:?} after {} ms",
            pairing.Status()?,
            t.elapsed().as_millis()
        );

        let device = WiFiDirectDevice::FromIdAsync(&info.Id()?)?.get()?;
        println!(
            "connected: {:?} after {} ms",
            device.ConnectionStatus()?,
            t.elapsed().as_millis()
        );
        let pairs = device.GetConnectionEndpointPairs()?;
        let mut peer = None;
        for i in 0..pairs.Size()? {
            let p = pairs.GetAt(i)?;
            let local = p.LocalHostName()?.DisplayName()?;
            let remote = p.RemoteHostName()?.DisplayName()?;
            println!("endpoints: here {local}, peer {remote}");
            peer.get_or_insert(remote);
        }

        if let Some(peer) = peer {
            // The peer's listener starts when it sees its address come up,
            // which is a moment after Windows reports the link.
            std::thread::sleep(Duration::from_secs(3));
            pull(&peer);
            push(&peer);
        }

        drop(device);
        publisher.Stop()?;
        let unpaired = info.Pairing()?.UnpairAsync()?.get()?;
        println!("unpaired: {:?}", unpaired.Status()?);
        Ok(())
    }

    /// A group any WPA2 station can join, with no P2P negotiation and no
    /// internet sharing behind it.
    fn legacy(ssid: &str, pass: &str, wait: u64) -> Result<()> {
        let publisher = WiFiDirectAdvertisementPublisher::new()?;
        let adv = publisher.Advertisement()?;
        adv.SetIsAutonomousGroupOwnerEnabled(true)?;
        let legacy = adv.LegacySettings()?;
        legacy.SetIsEnabled(true)?;
        legacy.SetSsid(&HSTRING::from(ssid))?;
        let cred = PasswordCredential::new()?;
        cred.SetPassword(&HSTRING::from(pass))?;
        legacy.SetPassphrase(&cred)?;
        publisher.StatusChanged(&TypedEventHandler::new(
            |_: Ref<WiFiDirectAdvertisementPublisher>,
             args: Ref<WiFiDirectAdvertisementPublisherStatusChangedEventArgs>| {
                if let Some(args) = args.as_ref() {
                    println!("group: {:?} ({:?})", args.Status()?, args.Error()?);
                }
                Ok(())
            },
        ))?;
        publisher.Start()?;
        println!("legacy group {ssid:?} up for {wait} s");
        std::thread::sleep(Duration::from_secs(wait));
        publisher.Stop()?;
        println!("group stopped");
        Ok(())
    }

    fn find(name: &str, wait: u64) -> Result<Option<DeviceInformation>> {
        let selector = WiFiDirectDevice::GetDeviceSelector2(
            WiFiDirectDeviceSelectorType::AssociationEndpoint,
        )?;
        let t = Instant::now();
        let (tx, rx) = mpsc::channel::<DeviceInformation>();
        let watcher = DeviceInformation::CreateWatcherAqsFilter(&selector)?;
        watcher.Added(&TypedEventHandler::new(
            move |_: Ref<DeviceWatcher>, info: Ref<DeviceInformation>| {
                if let Some(info) = info.as_ref() {
                    let _ = tx.send(info.clone());
                }
                Ok(())
            },
        ))?;
        watcher.EnumerationCompleted(&TypedEventHandler::new(
            move |_: Ref<DeviceWatcher>, _: Ref<windows::core::IInspectable>| {
                println!("enumeration pass done after {} ms", t.elapsed().as_millis());
                Ok(())
            },
        ))?;
        watcher.Start()?;
        let deadline = t + Duration::from_secs(wait);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(info) = rx.recv_timeout(left) else {
                break;
            };
            let n = info.Name()?.to_string();
            println!("seen {n:?} after {} ms", t.elapsed().as_millis());
            if n.to_lowercase().contains(name) {
                watcher.Stop()?;
                return Ok(Some(info));
            }
        }
        watcher.Stop()?;
        Ok(None)
    }

    fn pull(peer: &HSTRING) {
        let addr = format!("{peer}:5270");
        match TcpStream::connect(&addr) {
            Ok(mut s) => {
                let mut buf = vec![0u8; 1 << 18];
                let (mut n, t) = (0usize, Instant::now());
                while let Ok(r) = s.read(&mut buf) {
                    if r == 0 {
                        break;
                    }
                    n += r;
                }
                report("peer -> here", n, t.elapsed());
            }
            Err(e) => println!("pull from {addr}: {e}"),
        }
    }

    fn push(peer: &HSTRING) {
        let addr = format!("{peer}:5271");
        match TcpStream::connect(&addr) {
            Ok(mut s) => {
                let buf = vec![0u8; 1 << 18];
                let (mut n, t) = (0usize, Instant::now());
                while t.elapsed() < Duration::from_secs(8) {
                    match s.write(&buf) {
                        Ok(w) => n += w,
                        Err(e) => {
                            println!("push: {e}");
                            break;
                        }
                    }
                }
                let _ = s.shutdown(std::net::Shutdown::Write);
                report("here -> peer", n, t.elapsed());
            }
            Err(e) => println!("push to {addr}: {e}"),
        }
    }

    fn report(what: &str, bytes: usize, dt: Duration) {
        let s = dt.as_secs_f64();
        println!(
            "{what}: {:.1} MB in {s:.1} s = {:.2} MB/s ({:.0} Mbit/s)",
            bytes as f64 / 1e6,
            bytes as f64 / s / 1e6,
            bytes as f64 * 8.0 / s / 1e6
        );
    }
}
