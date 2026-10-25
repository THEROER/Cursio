//! This node's part in the mesh's one AirPlay receiver: the mode the flag
//! asks for, how fit this machine is to host, and hosting it when chosen.

use cursio_airplay::crypto::AppleCryptoProvider;
use cursio_airplay::{Receiver, ReceiverConfig, Resolution, VideoProfile};
use cursio_core::NodeIdentity;
use cursio_protocol::{Fitness, Quality, ServiceId};
use cursio_transport::discovery;
use cursio_transport::discovery::DiscoveryConfig;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// What `--airplay` was set to.
///
/// One value rather than the three booleans this used to be (`--airplay-screen`,
/// its `--airplay` alias, and `--no-airplay`, two of which had to be declared as
/// conflicting). There are three states and they are mutually exclusive, which
/// is exactly what one value expresses and three flags do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AirPlayMode {
    /// Do not take part: nothing is bound, nothing announced, and a peer's
    /// request to host is refused so it keeps looking.
    Off,
    /// Take part, asking the mesh for `wanted` if anything.
    ///
    /// The quality is a request made *of the mesh*, not a local setting: the
    /// receiver runs at the best any node asked for, on whichever machine can
    /// deliver it. A user asking for 1440p at their desk is asking for the
    /// picture to be that, and does not care which box decodes it.
    On { wanted: Option<Quality> },
}

/// The rate a profile means when it names only a size.
pub(crate) const DEFAULT_AIRPLAY_FPS: u16 = 60;

/// What the receiver runs at when nobody asked for anything in particular.
pub(crate) const DEFAULT_AIRPLAY_QUALITY: Quality = Quality::new(1920, 1080, DEFAULT_AIRPLAY_FPS);

/// How well this machine could hold the mesh's AirPlay receiver.
///
/// Two honest signals and no third. Whether the user let it take part, and
/// whether a hardware video decoder is installed — which the video module
/// answers by asking GStreamer for the elements it would actually use, rather
/// than by reading a GPU's name and guessing. A machine without one still
/// makes a fine control receiver; it is simply not where a picture should be
/// decoded, since this codebase refuses to fall back to software.
pub(crate) fn local_airplay_fitness(mode: AirPlayMode, port: u16) -> Fitness {
    if !mode.participates() {
        return Fitness::No;
    }
    // Port availability is part of capability, not a failure discovered after
    // election. Advertising a great decoder that cannot bind the receiver
    // would make every node consistently choose a host that can never start.
    if let Err(error) = std::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))) {
        tracing::info!(
            port,
            %error,
            "AirPlay port is unavailable here; this node will only request/view the mesh receiver"
        );
        return Fitness::No;
    }
    match cursio_airplay::video::dependency_warnings(cursio_airplay::video::VideoCodec::H264)
        .is_empty()
    {
        true => Fitness::Full,
        false => Fitness::ControlOnly,
    }
}

/// Map the protocol's numeric quality back to one of the receiver profiles.
pub(crate) fn resolution_for(quality: Quality) -> Resolution {
    if quality.height > 1080 {
        Resolution::Qhd
    } else {
        Resolution::FullHd
    }
}

impl AirPlayMode {
    /// Read `off`, `on`, or a profile like `1080p30`.
    pub(crate) fn parse(spec: &str) -> Result<Self, String> {
        let cleaned: String = spec
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !matches!(c, '-' | '_' | ' '))
            .collect();
        match cleaned.as_str() {
            "off" | "no" | "none" => return Ok(Self::Off),
            "on" | "control" => return Ok(Self::On { wanted: None }),
            // Named its own thing until the window became a flag of its own.
            // Refused rather than quietly re-read as `on`, which would leave a
            // user waiting for a picture that now needs asking for.
            "screen" => {
                return Err(
                    "`screen` no longer decides where the picture appears — pass \
                     --airplay-window on the machine it should appear on, and use --airplay \
                     on (or a profile like 1080p60) here"
                        .into(),
                )
            }
            _ => {}
        }
        parse_video_profile(&cleaned).map(|wanted| Self::On {
            wanted: Some(wanted),
        })
    }

    /// The quality this machine is asking the mesh for.
    pub(crate) fn wanted(self) -> Option<Quality> {
        match self {
            Self::On { wanted } => wanted,
            Self::Off => None,
        }
    }

    /// Whether this machine takes part in AirPlay at all.
    pub(crate) fn participates(self) -> bool {
        self != Self::Off
    }
}

/// Parse `1080p30`, `1440p`, `2k60` into a quality.
///
/// The rate may be separated by `:` or `@` or by nothing at all, because all
/// three are how people write it and none is ambiguous. Omitted, it is
/// [`DEFAULT_AIRPLAY_FPS`].
pub(crate) fn parse_video_profile(spec: &str) -> Result<Quality, String> {
    // Longest name first, so `1080p60` is not read as `1080` followed by the
    // nonsense `p60`.
    const NAMES: &[(&str, u16, u16)] = &[
        ("1080p", 1920, 1080),
        ("fullhd", 1920, 1080),
        ("1080", 1920, 1080),
        ("1440p", 2560, 1440),
        ("1440", 2560, 1440),
        ("qhd", 2560, 1440),
        ("2k", 2560, 1440),
    ];
    for &(name, width, height) in NAMES {
        let Some(rest) = spec.strip_prefix(name) else {
            continue;
        };
        let rest = rest.trim_start_matches([':', '@']);
        let rest = rest.strip_suffix("fps").unwrap_or(rest);
        // Anything but digits after the size means this was not the name we
        // took it for — `1080i` is a resolution nobody here supports, not a
        // 1080p at a frame rate called `i`. Fall through and let the error at
        // the end list what is actually accepted.
        if !rest.is_empty() && !rest.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let fps = match rest.is_empty() {
            true => DEFAULT_AIRPLAY_FPS,
            false => rest
                .parse::<u16>()
                .map_err(|_| format!("`{rest}` is not a frame rate"))?,
        };
        // The same bound the receiver's own profile enforces, checked here so
        // a typo is refused by the flag rather than by a bind three steps
        // later.
        if !(1..=120).contains(&fps) {
            return Err(format!("frame rate must be in 1..=120, got {fps}"));
        }
        return Ok(Quality::new(width, height, fps));
    }
    Err(format!(
        "`{spec}` is not one of off, control, screen, or a profile such as 1080p60, 1080p30, 2k30"
    ))
}

/// Answers a peer's request only when a hidden receiver is already bound.
pub(crate) struct AirPlayHost {
    pub(crate) state: cursio_core::service_session::ServiceState,
}

impl cursio_core::service_session::ServiceHost for AirPlayHost {
    fn accept(&mut self, service: ServiceId, _reason: &str) -> Result<(), String> {
        if service != ServiceId::AIRPLAY {
            return Err(format!("this machine does not host {service}"));
        }
        if self.state.is_hosted_here(service) {
            // `answer` records that this peer asked us; the placement watcher
            // then announces the receiver that is already serving.
            return Ok(());
        }
        Err("this machine has no bound AirPlay receiver to take over with".into())
    }
}

/// Everything hosting the mesh's AirPlay receiver needs from the command line.
pub(crate) struct AirPlayHosting {
    pub(crate) identity: NodeIdentity,
    pub(crate) port: u16,
    pub(crate) interface: Option<IpAddr>,
    /// Whether the mirrored picture appears on this machine.
    pub(crate) show_window: bool,
    /// Set when peers may watch the phone's screen through this node.
    pub(crate) tap: Option<cursio_airplay::MirrorTap>,
    /// False when another cursio on this machine holds the local claim — a
    /// question no peer can answer, since both processes are the same peer.
    pub(crate) may_announce: bool,
    pub(crate) state: cursio_core::service_session::ServiceState,
    pub(crate) reasons: Arc<std::sync::Mutex<std::collections::HashMap<ServiceId, String>>>,
}

/// Hold the receiver for as long as the mesh wants it here, rebuilding it when
/// the agreed quality changes, until the mesh itself ends.
///
/// A loop rather than a single bind, because the quality is the *mesh's* and
/// can change under this machine: a peer joining and asking for 1440p is a
/// request this node has to honour if it is the one holding the receiver. The
/// mesh future is pinned across iterations — it is the thing that must not be
/// restarted, since dropping it would take every peer session with it.
pub(crate) async fn run_airplay(
    opts: AirPlayHosting,
    mesh: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    tokio::pin!(mesh);
    let mut running = local_airplay_placement(&opts.state, opts.identity.peer_id);
    loop {
        // Asked before every bind, not once: a peer that joined during the
        // last receiver's life may have raised what the mesh wants.
        let announce = opts.may_announce && running.selected;
        let receiver = match bind_airplay(
            &opts.identity,
            AirPlayOpts {
                announce,
                port: opts.port,
                interface: opts.interface,
                quality: running.quality,
                show_window: opts.show_window && running.selected,
                tap: opts.tap.clone(),
            },
        )
        .await
        {
            Ok(receiver) => receiver,
            // A busy port — macOS' own receiver holds 7000 — must not take the
            // mesh down, and must not end in silence either. The machine says
            // what it could not do, and every session asks its peer to host it
            // instead; the phone then mirrors to that peer, which streams the
            // picture back here through the ordinary window path. Which machine
            // holds the socket stops being something the user has to know.
            Err(error) => {
                tracing::warn!(
                    %error,
                    "cannot host AirPlay here; asking a peer to host it for the mesh"
                );
                opts.reasons
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(ServiceId::AIRPLAY, error.to_string());
                opts.state.needs_host(ServiceId::AIRPLAY);
                return mesh.await;
            }
        };

        // Published before serving, so a peer's query is answered from what is
        // actually bound rather than from what was asked for on the command
        // line.
        if let Ok(addr) = receiver.local_addr() {
            let reachable = SocketAddr::new(
                discovery::guess_lan_ipv4().unwrap_or_else(|| IpAddr::from([127, 0, 0, 1])),
                addr.port(),
            );
            opts.state
                .now_hosting(ServiceId::AIRPLAY, reachable, running.quality);
        }

        let follower = follow_airplay_answers(
            receiver.announcer(),
            opts.state.clone(),
            opts.identity.peer_id,
            opts.may_announce,
        );
        let wanted = tokio::select! {
            result = &mut mesh => return result,
            result = receiver.serve() => return result.map_err(Into::into),
            wanted = follower => wanted,
        };

        // Only a change gets here, and rebuilding costs the viewer a second of
        // black screen — so say why in the log, where a user watching it happen
        // will look.
        tracing::info!(
            from = ?running.quality.map(|q| q.to_string()),
            to = ?wanted.quality.map(|q| q.to_string()),
            was_host = running.selected,
            is_host = wanted.selected,
            "the mesh changed AirPlay placement; restarting the receiver"
        );
        opts.state.no_longer_hosting(ServiceId::AIRPLAY);
        running = wanted;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalAirPlayPlacement {
    selected: bool,
    quality: Option<Quality>,
}

/// What this machine should run from the mesh's deterministic placement.
pub(crate) fn local_airplay_placement(
    state: &cursio_core::service_session::ServiceState,
    local: cursio_protocol::PeerId,
) -> LocalAirPlayPlacement {
    if state.was_asked_to_host(ServiceId::AIRPLAY) && state.is_hosted_here(ServiceId::AIRPLAY) {
        return LocalAirPlayPlacement {
            selected: true,
            quality: Some(
                state
                    .placement()
                    .and_then(|placement| placement.quality)
                    .unwrap_or(DEFAULT_AIRPLAY_QUALITY),
            ),
        };
    }
    match state.placement() {
        Some(placement) if placement.host == local => LocalAirPlayPlacement {
            selected: true,
            quality: Some(placement.quality.unwrap_or(DEFAULT_AIRPLAY_QUALITY)),
        },
        _ => LocalAirPlayPlacement {
            selected: false,
            quality: None,
        },
    }
}

/// Keep the mDNS record in step with what peers say they are hosting.
///
/// Polled rather than pushed: the answer arrives on whichever peer session
/// happens to be up, and there may be several coming and going, so the state
/// they all write is the one thing that outlives them. A second is far below
/// what a user notices between plugging a machine in and finding one Cursio in
/// the list, and costs one atomic read.
pub(crate) async fn follow_airplay_answers(
    announcer: cursio_airplay::Announcer,
    state: cursio_core::service_session::ServiceState,
    local: cursio_protocol::PeerId,
    // Whether this process may announce at all. False when another cursio on
    // this machine holds the claim — a question no peer can answer, since both
    // processes are the same peer, so it has to be carried in rather than
    // rediscovered here.
    may_announce: bool,
) -> LocalAirPlayPlacement {
    const POLL: Duration = Duration::from_secs(1);
    let running = local_airplay_placement(&state, local);
    // How long a machine has to look like the host before it acts on it.
    //
    // A peer's offer is forgotten the instant its link drops, so a reconnect —
    // which the mesh attempts every two seconds — briefly looks exactly like
    // that peer being gone for good. Acting on it immediately meant every blink
    // of the link cost two receiver restarts: this node took the duty, the peer
    // came back a second later and took it again.
    //
    // Only *taking* the duty waits. Standing down stays immediate, because the
    // failure it prevents is worse: two `Cursio` in an iPhone's picker, which
    // is what the user sees and has to choose between.
    const SETTLE: u32 = 5;
    // How many consecutive polls have agreed on `settled`. Counted against the
    // reading itself rather than against what is running, so an announcement
    // that drifted out of step with the placement is still corrected — a node
    // that bound silently because a peer looked reachable, and then turned out
    // to be the host after all, would otherwise stay silent forever.
    let mut settled = local_airplay_placement(&state, local);
    let mut steady = 0_u32;
    loop {
        tokio::time::sleep(POLL).await;
        let wanted = local_airplay_placement(&state, local);
        steady = match wanted == settled {
            true => steady.saturating_add(1),
            false => {
                settled = wanted;
                1
            }
        };

        let should_announce = may_announce && wanted.selected;
        if !should_announce && announcer.is_announced() {
            tracing::info!(
                addr = ?state.host_address(ServiceId::AIRPLAY),
                "the mesh selected another AirPlay host; standing down"
            );
            announcer.withdraw();
        } else if should_announce && !announcer.is_announced() && steady >= SETTLE {
            if let Err(error) = announcer.announce() {
                tracing::warn!(%error, "could not announce the selected AirPlay host");
            }
        }

        // A host change also rebuilds when the quality is the same: local
        // window/tap behaviour belongs to the selected receiver, not merely to
        // a numeric profile.
        if wanted == running {
            continue;
        }
        // Losing the duty is acted on at once; gaining it waits out a peer that
        // is merely reconnecting.
        if !wanted.selected || steady >= SETTLE {
            return wanted;
        }
        if steady == 1 {
            tracing::debug!(
                seconds = SETTLE,
                "this machine would take AirPlay; waiting in case the peer is reconnecting"
            );
        }
    }
}

pub(crate) struct AirPlayOpts {
    /// Whether to put this receiver in an iPhone's picker. False on every node
    /// but the one that won the election; see [`claims_airplay`].
    announce: bool,
    port: u16,
    interface: Option<IpAddr>,
    /// What the mesh agreed the picture should be, or `None` for a
    /// control-only receiver that answers an iPhone and decodes nothing.
    quality: Option<Quality>,
    /// Whether the picture appears on this machine.
    show_window: bool,
    /// Set when peers should be able to watch the mirrored screen too.
    tap: Option<cursio_airplay::MirrorTap>,
}

pub(crate) async fn bind_airplay(
    identity: &NodeIdentity,
    options: AirPlayOpts,
) -> anyhow::Result<Receiver> {
    // A control-only receiver still needs a profile to describe itself with in
    // `/info`; it simply never decodes anything.
    let quality = options.quality.unwrap_or(DEFAULT_AIRPLAY_QUALITY);
    let profile = VideoProfile::new(resolution_for(quality), quality.fps)?;
    if options.quality.is_some() {
        for warning in
            cursio_airplay::video::dependency_warnings(cursio_airplay::video::VideoCodec::H264)
        {
            tracing::warn!("{warning}");
        }
    }
    let crypto = Arc::new(AppleCryptoProvider::new());
    let config = ReceiverConfig::new(
        AIRPLAY_RECEIVER_NAME,
        identity.peer_id,
        SocketAddr::from(([0, 0, 0, 0], options.port)),
        profile,
    )?
    .with_pairing_public_key(crypto.public_key());
    let discovery = options
        .interface
        .map_or_else(DiscoveryConfig::default, DiscoveryConfig::pinned_to);
    let mut receiver = Receiver::bind(config, discovery, crypto, options.announce)
        .await?
        .with_screen_enabled(options.show_window);
    if let Some(tap) = options.tap {
        receiver = receiver.with_mirror_tap(tap);
    }
    tracing::info!(
        address = %receiver.local_addr()?,
        screen = options.show_window,
        quality = %quality,
        announced = options.announce,
        "mesh AirPlay RTSP receiver is available"
    );
    Ok(receiver)
}

/// What an iPhone shows in its Screen Mirroring list.
///
/// One name for the whole mesh, not one per machine. Which machine actually
/// decodes the stream is Cursio's business — the phone is picking "my
/// desktop", and offering it `Cursio - macbook` beside `Cursio - desktop`
/// makes the user choose between two answers to a question they did not ask.
/// Exactly one node announces it; see [`claims_airplay`].
pub(crate) const AIRPLAY_RECEIVER_NAME: &str = "Cursio";

/// Hold the machine-wide claim on announcing AirPlay, if it is free.
///
/// Two `cursio` processes on one machine put two `Cursio` entries in an
/// iPhone's list, and no amount of talking to *peers* fixes that: both are the
/// same peer, with the same id, so the mesh election ranks them identically
/// and both conclude they won. The question "is one already running here" has
/// a local answer, and a lock file is it.
///
/// The returned file must be held for as long as the process announces —
/// dropping it releases the lock, and closing it on exit is what lets the next
/// run take over even after a crash, since the kernel releases the lock with
/// the descriptor rather than leaving a stale file behind.
pub(crate) fn claim_local_airplay() -> Option<std::fs::File> {
    take_lock(&cursio_log::log_dir().join("airplay.lock"))
}

/// Take an exclusive lock on `path`, or report that somebody else holds it.
pub(crate) fn take_lock(path: &std::path::Path) -> Option<std::fs::File> {
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            // Not fatal: without the lock the worst case is the duplicate entry
            // this is meant to prevent, which beats refusing to run at all.
            tracing::warn!(%error, path = %path.display(), "cannot take the AirPlay lock");
            return None;
        }
    };
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => {
            tracing::info!(
                "another cursio on this machine is already announcing AirPlay; not adding a second"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two processes on one machine are the same peer, with the same id, so
    /// the mesh election ranks them identically and both conclude they won.
    /// The lock is the only thing that tells them apart.
    #[test]
    fn only_one_holder_of_the_local_claim_at_a_time() {
        let path = std::env::temp_dir().join(format!("cursio-test-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let first = take_lock(&path).expect("the first caller takes it");
        assert!(
            take_lock(&path).is_none(),
            "a second caller must be refused"
        );

        // Released with the descriptor, so the next run takes over even if the
        // holder crashed without cleaning up.
        drop(first);
        assert!(take_lock(&path).is_some(), "releasing must hand it on");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_mode_separates_participation_from_the_window_flag() {
        assert_eq!(AirPlayMode::parse("off").unwrap(), AirPlayMode::Off);
        assert!(!AirPlayMode::parse("off").unwrap().participates());
        assert_eq!(
            AirPlayMode::parse("control").unwrap(),
            AirPlayMode::On { wanted: None }
        );
        assert!(AirPlayMode::parse("screen").is_err());
    }

    /// A profile in place of `screen` says both things at once, which is the
    /// whole reason `--res` and `--fps` stopped being separate flags.
    #[test]
    fn a_profile_requests_mesh_quality_without_implying_a_window() {
        let cases = [
            ("1080p30", Resolution::FullHd, 30),
            ("1080p", Resolution::FullHd, 60),
            ("2k", Resolution::Qhd, 60),
            ("1440p24", Resolution::Qhd, 24),
        ];
        for (spec, resolution, fps) in cases {
            let quality = AirPlayMode::parse(spec)
                .unwrap_or_else(|e| panic!("{spec}: {e}"))
                .wanted()
                .unwrap_or_else(|| panic!("{spec} should request a quality"));
            assert_eq!(
                (resolution_for(quality), quality.fps),
                (resolution, fps),
                "{spec}"
            );
        }
    }

    /// Three ways of writing the same thing, because all three are how people
    /// write it and none of them is ambiguous.
    #[test]
    fn the_rate_may_be_separated_or_not() {
        for spec in ["1080p30", "1080p:30", "1080p@30", "1080P-30", "1080p:30fps"] {
            let quality = AirPlayMode::parse(spec).unwrap().wanted().unwrap();
            assert_eq!(quality.fps, 30, "{spec}");
        }
    }

    #[test]
    fn a_mode_that_is_not_a_mode_names_what_it_could_be() {
        let error = AirPlayMode::parse("1080i").unwrap_err();
        assert!(error.contains("1080p60"), "{error}");
        assert!(error.contains("off"), "{error}");
        // Out of range is the profile's own complaint, not this one.
        assert!(AirPlayMode::parse("1080p999").is_err());
    }

    /// The phone is picking "my desktop", not a machine. A name carrying the
    /// hostname is what produced two entries for one mesh.
    #[test]
    fn the_mesh_offers_one_name_whatever_the_hostname_is() {
        assert_eq!(AIRPLAY_RECEIVER_NAME, "Cursio");
        assert!(!AIRPLAY_RECEIVER_NAME.contains('-'));
    }
}
