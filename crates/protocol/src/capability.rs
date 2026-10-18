//! What a node *is* and what it can *do*, exchanged in [`Hello`](crate::Hello).
//!
//! Before this, a session knew only the peer's protocol version, id and name —
//! nothing about which subsystems it had. Modules were enabled by CLI flags on
//! each machine independently, so `--audio` on one side and not the other left
//! the link silently half-configured: one side advertising streams into a void,
//! with no log line saying why nothing played.
//!
//! Two things fix that, and both belong in the greeting because both must be
//! known before any module starts:
//!
//! - [`ModuleAdvert`] — "I have this module, at this version, in this role". A
//!   module runs on a link only when **both** sides advertise it, so a
//!   one-sided configuration becomes a startup log line instead of a mystery.
//! - [`DeviceKind`] — what sort of machine this is. A phone is not a small
//!   desktop: it has no screen edge bordering the peer, so it can't take part in
//!   cursor ownership at all. Carrying that explicitly is what keeps
//!   "is this thing a cursor destination?" out of the ownership loop as a
//!   special case.
//!
//! ## Unknown modules are ignored, never rejected
//!
//! [`ModuleId`] is an open `u16` namespace, and a peer advertising an id we
//! don't know is not an error — we simply don't negotiate that module. This is
//! the whole point: a newer peer can grow a module without every older peer
//! refusing to talk to it. Only the *session* handshake
//! ([`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION)) is version-locked; module
//! vocabularies version themselves via [`ModuleAdvert::version`].
//!
//! See `docs/platform-design.md`.

use serde::{Deserialize, Serialize};

/// Stable identifier for a subsystem multiplexed onto the session channel.
///
/// Values are assigned once and never reused — an id is part of the wire format
/// the moment any build ships with it. Kept in one place (here) so a new module
/// can't silently collide with an existing one.
///
/// Also the routing key for [`Message::Module`](crate::Message::Module): an
/// enveloped frame is handed to the module whose id it carries, so
/// [`SessionModule`] implementors never see another module's traffic.
///
/// [`SessionModule`]: https://docs.rs/cursio-core
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModuleId(pub u16);

impl ModuleId {
    /// Shared-cursor input: relative motion, keys, buttons, edge crossings.
    pub const INPUT: ModuleId = ModuleId(1);
    /// Networked per-app audio bus.
    pub const AUDIO: ModuleId = ModuleId(2);
    /// System clipboard sharing.
    pub const CLIPBOARD: ModuleId = ModuleId(3);
    /// Window transfer (pixels stream, app stays on the host).
    pub const WINDOW: ModuleId = ModuleId(4);
    /// Filesystem sharing: the peer's volumes, mounted here as ordinary folders.
    pub const FILES: ModuleId = ModuleId(5);
    /// Asking a peer to carry a duty this machine cannot; see
    /// [`service`](crate::service).
    pub const SERVICE: ModuleId = ModuleId(6);
    /// USB device sharing: a device on one machine's port, attached from
    /// another; see [`usb`](crate::usb).
    pub const USB: ModuleId = ModuleId(7);
    /// Handing new builds of Cursio between machines; see
    /// [`update`](crate::update).
    pub const UPDATE: ModuleId = ModuleId(8);
    /// Sending a link, text or file to another machine to open, paste or
    /// keep; see [`share`](crate::share).
    pub const SHARE: ModuleId = ModuleId(9);
    /// A phone's notifications, shown on the machine the user is at; see
    /// [`notify`](crate::notify).
    pub const NOTIFY: ModuleId = ModuleId(10);
    /// A phone's text messages, read and sent from a desktop; see
    /// [`messages`](crate::messages).
    pub const MESSAGES: ModuleId = ModuleId(11);
    /// A phone's calls, answered, placed and ended from a computer; see
    /// [`calls`](crate::calls).
    pub const CALLS: ModuleId = ModuleId(12);

    /// Human-readable name, for logs and diagnostics. Unknown ids are rendered
    /// numerically rather than hidden — that is exactly the case worth seeing in
    /// a log when an older build meets a newer peer.
    pub fn name(&self) -> &'static str {
        match *self {
            ModuleId::INPUT => "input",
            ModuleId::AUDIO => "audio",
            ModuleId::CLIPBOARD => "clipboard",
            ModuleId::WINDOW => "window",
            ModuleId::FILES => "files",
            ModuleId::SERVICE => "service",
            ModuleId::USB => "usb",
            ModuleId::UPDATE => "update",
            ModuleId::SHARE => "share",
            ModuleId::NOTIFY => "notify",
            ModuleId::MESSAGES => "messages",
            ModuleId::CALLS => "calls",
            _ => "unknown",
        }
    }
}

impl std::fmt::Display for ModuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            "unknown" => write!(f, "module#{}", self.0),
            name => write!(f, "{name}"),
        }
    }
}

/// What sort of machine a node is.
///
/// Not cosmetic: it decides whether a node can hold the shared cursor at all.
/// A phone or tablet joins the mesh as an input *device* and a clipboard/file
/// peer, but the cursor never travels *to* it — there is no screen edge to cross
/// and nothing to inject into. See [`DeviceKind::takes_cursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Desktop,
    Laptop,
    Phone,
    Tablet,
    /// No display and no local input — a server or a headless box. Useful for
    /// audio/clipboard/file modules; never a cursor destination.
    Headless,
}

impl DeviceKind {
    /// Can the shared cursor move *onto* this machine?
    ///
    /// True only where there is a screen to enter and an injector to drive it.
    /// The ownership loop consults this instead of special-casing device types,
    /// which is why the kind is negotiated rather than inferred.
    pub fn takes_cursor(&self) -> bool {
        matches!(self, DeviceKind::Desktop | DeviceKind::Laptop)
    }
}

/// Which half of an asymmetric module a node implements.
///
/// Most modules are symmetric (clipboard: both sides copy and paste), but not
/// all: a phone touchpad only ever *produces* input, and a headless box only
/// ever *consumes* audio. Negotiation pairs a `Source` with a `Sink`; two
/// `Source`s have nothing to say to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModuleRole {
    /// Produces only (phone touchpad emitting input).
    Source,
    /// Consumes only (headless box rendering audio).
    Sink,
    /// Does both — the common case.
    Both,
}

impl ModuleRole {
    /// Do these two roles have anything to exchange?
    ///
    /// `Both` pairs with anything; otherwise the roles must be opposite. Two
    /// `Source`s (or two `Sink`s) share a module id but no direction of flow, so
    /// starting the module on either side would be pointless.
    pub fn compatible_with(&self, peer: ModuleRole) -> bool {
        matches!(
            (self, peer),
            (ModuleRole::Both, _)
                | (_, ModuleRole::Both)
                | (ModuleRole::Source, ModuleRole::Sink)
                | (ModuleRole::Sink, ModuleRole::Source)
        )
    }
}

/// "I have this module, at this version, in this role."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleAdvert {
    pub id: ModuleId,
    /// Version of *this module's* message vocabulary, independent of
    /// [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION). Bumped when the module's
    /// own types change shape; peers run the module only on an exact match, so
    /// an outdated audio module is skipped while clipboard keeps working.
    pub version: u16,
    pub role: ModuleRole,
}

impl ModuleAdvert {
    pub fn new(id: ModuleId, version: u16, role: ModuleRole) -> Self {
        Self { id, version, role }
    }

    /// A symmetric module at version 1 — the shape most modules start in.
    pub fn symmetric(id: ModuleId, version: u16) -> Self {
        Self::new(id, version, ModuleRole::Both)
    }
}

/// Why a locally-available module did not start on a link.
///
/// Carried so the reason reaches a log line (and later a UI) instead of the
/// module simply being absent: "audio is off because the peer doesn't have it"
/// and "audio is off because their build is older" are very different problems
/// to the person trying to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The peer never advertised this module.
    PeerLacks,
    /// Both have it, but their vocabularies differ.
    VersionMismatch { local: u16, peer: u16 },
    /// Both have it at the same version, but the roles don't pair up (two
    /// sources, or two sinks).
    RoleMismatch { local: ModuleRole, peer: ModuleRole },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::PeerLacks => write!(f, "peer does not have it"),
            SkipReason::VersionMismatch { local, peer } => {
                write!(f, "version mismatch (local v{local}, peer v{peer})")
            }
            SkipReason::RoleMismatch { local, peer } => {
                write!(f, "incompatible roles (local {local:?}, peer {peer:?})")
            }
        }
    }
}

/// The outcome of comparing our modules against the peer's.
#[derive(Debug, Clone, Default)]
pub struct Negotiated {
    /// Modules both sides agreed on. These are the ones to build.
    pub agreed: Vec<ModuleId>,
    /// Modules we have but won't run here, each with why. Logged at startup so
    /// a half-configured link explains itself.
    pub skipped: Vec<(ModuleId, SkipReason)>,
}

impl Negotiated {
    pub fn is_agreed(&self, id: ModuleId) -> bool {
        self.agreed.contains(&id)
    }
}

/// Decide which modules run on this link.
///
/// Symmetric by construction: both sides run this over the same two sets and
/// reach the same `agreed` list without negotiating further. That matters
/// because there is no coordinator to break a tie — the rule *is* the agreement.
///
/// Modules the peer advertises and we don't are not reported: we have nothing to
/// run, and an id we don't recognise is explicitly not an error (that is what
/// lets a newer peer grow modules without breaking older ones).
pub fn negotiate(local: &[ModuleAdvert], peer: &[ModuleAdvert]) -> Negotiated {
    let mut out = Negotiated::default();
    for mine in local {
        let Some(theirs) = peer.iter().find(|p| p.id == mine.id) else {
            out.skipped.push((mine.id, SkipReason::PeerLacks));
            continue;
        };
        if mine.version != theirs.version {
            out.skipped.push((
                mine.id,
                SkipReason::VersionMismatch {
                    local: mine.version,
                    peer: theirs.version,
                },
            ));
            continue;
        }
        if !mine.role.compatible_with(theirs.role) {
            out.skipped.push((
                mine.id,
                SkipReason::RoleMismatch {
                    local: mine.role,
                    peer: theirs.role,
                },
            ));
            continue;
        }
        out.agreed.push(mine.id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(id: ModuleId) -> ModuleAdvert {
        ModuleAdvert::symmetric(id, 1)
    }

    /// The case this whole module exists for: `--audio` on one side only used to
    /// be silent. Now it is a skip with a reason attached.
    #[test]
    fn one_sided_module_is_skipped_with_a_reason() {
        let local = [sym(ModuleId::AUDIO), sym(ModuleId::CLIPBOARD)];
        let peer = [sym(ModuleId::CLIPBOARD)];

        let n = negotiate(&local, &peer);

        assert_eq!(n.agreed, vec![ModuleId::CLIPBOARD]);
        assert_eq!(n.skipped, vec![(ModuleId::AUDIO, SkipReason::PeerLacks)]);
    }

    /// Both sides must reach the same conclusion without talking further —
    /// there is no coordinator to break a tie.
    #[test]
    fn negotiation_is_symmetric() {
        let a = [sym(ModuleId::INPUT), sym(ModuleId::AUDIO)];
        let b = [sym(ModuleId::AUDIO), sym(ModuleId::WINDOW)];

        assert_eq!(negotiate(&a, &b).agreed, negotiate(&b, &a).agreed);
    }

    /// A stale module must not take the rest of the link down with it.
    #[test]
    fn a_version_mismatch_skips_only_that_module() {
        let local = [
            ModuleAdvert::symmetric(ModuleId::AUDIO, 2),
            sym(ModuleId::CLIPBOARD),
        ];
        let peer = [
            ModuleAdvert::symmetric(ModuleId::AUDIO, 1),
            sym(ModuleId::CLIPBOARD),
        ];

        let n = negotiate(&local, &peer);

        assert_eq!(n.agreed, vec![ModuleId::CLIPBOARD]);
        assert_eq!(
            n.skipped,
            vec![(
                ModuleId::AUDIO,
                SkipReason::VersionMismatch { local: 2, peer: 1 }
            )]
        );
    }

    /// Forward compatibility is the default: an id we've never heard of is not
    /// an error, it's just a module we don't run.
    #[test]
    fn an_unknown_peer_module_is_ignored_not_rejected() {
        let local = [sym(ModuleId::CLIPBOARD)];
        let peer = [sym(ModuleId::CLIPBOARD), sym(ModuleId(9999))];

        let n = negotiate(&local, &peer);

        assert_eq!(n.agreed, vec![ModuleId::CLIPBOARD]);
        assert!(
            n.skipped.is_empty(),
            "an unknown peer module was reported as a problem"
        );
    }

    /// A phone touchpad (Source) pairs with a desktop injector (Sink); two
    /// phones have nothing to exchange.
    #[test]
    fn asymmetric_modules_pair_only_in_opposite_roles() {
        let phone = [ModuleAdvert::new(ModuleId::INPUT, 1, ModuleRole::Source)];
        let desktop = [ModuleAdvert::new(ModuleId::INPUT, 1, ModuleRole::Sink)];
        assert_eq!(negotiate(&phone, &desktop).agreed, vec![ModuleId::INPUT]);

        let n = negotiate(&phone, &phone);
        assert!(
            n.agreed.is_empty(),
            "two sources negotiated a module with no direction of flow"
        );
        assert!(matches!(n.skipped[0].1, SkipReason::RoleMismatch { .. }));
    }

    /// The cursor must never be handed to a machine with no screen to enter.
    #[test]
    fn only_screened_devices_take_the_cursor() {
        assert!(DeviceKind::Desktop.takes_cursor());
        assert!(DeviceKind::Laptop.takes_cursor());
        assert!(!DeviceKind::Phone.takes_cursor());
        assert!(!DeviceKind::Tablet.takes_cursor());
        assert!(!DeviceKind::Headless.takes_cursor());
    }
}
