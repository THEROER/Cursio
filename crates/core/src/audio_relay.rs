//! Sound this machine produces for a peer to play: a phone's call voice and
//! its music, heard over a Bluetooth link here but wanted where the user is.
//!
//! The audio module knew two kinds of source, both captured from the local
//! graph: applications routed into a peer's device, and microphones. A call
//! carried over hands-free is neither — its PCM comes off a socket — and
//! sending it through the graph into a peer's Intent Sink would need this
//! machine to know the peer's default output and link a node to it. So the
//! module takes one more kind of source: PCM handed over in-process and sent
//! to one peer as an ordinary application stream. The peer plays it on its
//! default output like any other application of ours, under the label given
//! here, with the same Opus, priority class and rate control — no new message
//! on the wire, so a peer of any build that runs the module can play it.
//!
//! A relay that wants a microphone gets the peer's: the module tees the
//! decoded PCM of the first microphone the peer offered, and keeps that
//! stream demanded for as long as the relay lives, whether or not anything
//! here records from its mirror.
//!
//! Each link's audio module registers here while it runs, so a producer can
//! tell which peers it can reach and what they are called.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cursio_protocol::{AudioChunk, PeerId};
use tokio::sync::mpsc;

/// What a call's voice is called on the machine that plays it. The prefix is
/// how that machine knows the stream is a call's voice and for which phone.
pub const CALL_LABEL: &str = "Call · ";

/// The label a phone's call voice carries to the peer playing it.
pub fn call_label(phone: &str) -> String {
    format!("{CALL_LABEL}{phone}")
}

/// The phone a stream labelled by [`call_label`] carries a call of.
pub fn call_of(label: &str) -> Option<&str> {
    label.strip_prefix(CALL_LABEL)
}

/// One relay's shared state, as the audio module of the peer's link sees it.
pub(crate) struct Relay {
    pub(crate) peer: PeerId,
    pub(crate) label: String,
    pub(crate) wants_mic: bool,
    pcm: Mutex<Option<mpsc::UnboundedReceiver<AudioChunk>>>,
    pub(crate) mic: mpsc::UnboundedSender<AudioChunk>,
    closed: AtomicBool,
}

impl Relay {
    fn take_pcm(&self) -> Option<mpsc::UnboundedReceiver<AudioChunk>> {
        lock(&self.pcm).take()
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// Held by the producer; dropping it ends the stream on the peer.
pub struct RelayHandle {
    pcm: mpsc::UnboundedSender<AudioChunk>,
    mic: mpsc::UnboundedReceiver<AudioChunk>,
    relay: Arc<Relay>,
}

impl RelayHandle {
    pub fn peer(&self) -> PeerId {
        self.relay.peer
    }

    /// Queue PCM in the module's format (48 kHz stereo `f32`, as a capture
    /// produces it). Dropped quietly when the link is gone: the producer
    /// decides where sound goes, not whether a socket is open.
    pub fn play(&self, chunk: AudioChunk) {
        let _ = self.pcm.send(chunk);
    }

    /// The peer's microphone, decoded, in the same format.
    pub fn mic(&mut self) -> &mut mpsc::UnboundedReceiver<AudioChunk> {
        &mut self.mic
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.relay.closed.store(true, Ordering::Release);
        lock(&state().relays).retain(|r| !Arc::ptr_eq(r, &self.relay));
    }
}

struct LinkEntry {
    token: u64,
    peer: PeerId,
    name: String,
}

#[derive(Default)]
struct State {
    relays: Mutex<Vec<Arc<Relay>>>,
    links: Mutex<Vec<LinkEntry>>,
}

fn state() -> &'static State {
    static STATE: OnceLock<State> = OnceLock::new();
    STATE.get_or_init(State::default)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A link's audio module, registered while it runs.
pub(crate) struct LinkMark(u64);

impl LinkMark {
    pub(crate) fn new(peer: PeerId, name: &str) -> Self {
        let token: u64 = rand::random();
        lock(&state().links).push(LinkEntry {
            token,
            peer,
            name: name.to_string(),
        });
        LinkMark(token)
    }
}

impl Drop for LinkMark {
    fn drop(&mut self) {
        lock(&state().links).retain(|l| l.token != self.0);
    }
}

/// The name of `peer` when sound can reach it now.
pub fn reachable(peer: PeerId) -> Option<String> {
    lock(&state().links)
        .iter()
        .find(|l| l.peer == peer)
        .map(|l| l.name.clone())
}

/// Start sending sound to `peer`, shown there as `label`.
pub fn open(peer: PeerId, label: &str, wants_mic: bool) -> RelayHandle {
    let (pcm_tx, pcm_rx) = mpsc::unbounded_channel();
    let (mic_tx, mic_rx) = mpsc::unbounded_channel();
    let relay = Arc::new(Relay {
        peer,
        label: label.to_string(),
        wants_mic,
        pcm: Mutex::new(Some(pcm_rx)),
        mic: mic_tx,
        closed: AtomicBool::new(false),
    });
    lock(&state().relays).push(relay.clone());
    RelayHandle {
        pcm: pcm_tx,
        mic: mic_rx,
        relay,
    }
}

/// Relays for `peer` that no link has taken yet. Taking one claims its PCM,
/// so a second link to the same peer (a reconnect overlapping the old one)
/// does not split the stream.
pub(crate) fn unclaimed(peer: PeerId) -> Vec<(Arc<Relay>, mpsc::UnboundedReceiver<AudioChunk>)> {
    lock(&state().relays)
        .iter()
        .filter(|r| r.peer == peer)
        .filter_map(|r| r.take_pcm().map(|pcm| (r.clone(), pcm)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(seq: u32) -> AudioChunk {
        AudioChunk {
            id: 0,
            seq,
            timestamp: 0,
            data: vec![0; 8],
        }
    }

    #[test]
    fn a_relay_is_taken_once_by_the_link_to_its_peer() {
        let laptop = PeerId::from_u128(rand::random());
        let handle = open(laptop, "Call", true);
        assert!(unclaimed(PeerId::from_u128(rand::random())).is_empty());
        let mut taken = unclaimed(laptop);
        assert_eq!(taken.len(), 1);
        assert!(unclaimed(laptop).is_empty());
        handle.play(chunk(1));
        assert_eq!(taken[0].1.try_recv().unwrap().seq, 1);
    }

    #[test]
    fn dropping_the_handle_closes_the_relay_for_its_link() {
        let peer = PeerId::from_u128(rand::random());
        let handle = open(peer, "Music", false);
        let taken = unclaimed(peer);
        drop(handle);
        assert!(taken[0].0.closed());
    }

    #[test]
    fn the_peers_microphone_reaches_the_producer() {
        let peer = PeerId::from_u128(rand::random());
        let mut handle = open(peer, "Call", true);
        let taken = unclaimed(peer);
        taken[0].0.mic.send(chunk(7)).unwrap();
        assert_eq!(handle.mic().try_recv().unwrap().seq, 7);
    }

    #[test]
    fn a_peer_is_reachable_while_its_link_runs_audio() {
        let peer = PeerId::from_u128(rand::random());
        assert_eq!(reachable(peer), None);
        let mark = LinkMark::new(peer, "Lightmoon");
        assert_eq!(reachable(peer).as_deref(), Some("Lightmoon"));
        drop(mark);
        assert_eq!(reachable(peer), None);
    }

    #[test]
    fn a_call_label_names_its_phone() {
        assert_eq!(call_of(&call_label("POCO X7 Pro")), Some("POCO X7 Pro"));
        assert_eq!(call_of("Firefox"), None);
    }
}
