//! Applications' builds, and messages between copies of one application,
//! carried between machines (`docs/app-sdk-design.md`).
//!
//! The update lane's messages are reused byte for byte — offer, want, chunk,
//! done — each wrapped with the application it is about. The update lane
//! itself is not: its bytes are frozen so that two Cursio builds of any age
//! can still update each other, and a second meaning in it would be a second
//! thing every old build has to understand. Here a build that does not know
//! this module simply does not advertise it.
//!
//! **Two kinds in one envelope.** [`AppsMsg::payload`] holds either an
//! update-lane message, which starts with the lane's own magic (`CUPD`), or
//! an [`AppsWire`] message, which starts with [`WIRE_MAGIC`]. The module's
//! version stays 1: peers run a module only on an exact version match, and a
//! bump would have stopped builds crossing between an updated machine and one
//! not yet updated. A node that predates [`AppsWire`] finds no lane message
//! in such a payload and skips it, as it skips any kind it does not know.

use serde::{Deserialize, Serialize};

use crate::update::UpdateMsg;
use crate::Priority;

/// What an [`AppsWire`] payload starts with; the update lane's is `CUPD`.
pub const WIRE_MAGIC: [u8; 4] = *b"CAPW";

/// A message between copies larger than this rides the bulk class: it is
/// still a message, but a big one must not hold up the small ones of other
/// applications.
pub const BULK_FROM: usize = 16 * 1024;

/// One message about one application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsMsg {
    /// The application's id, as it registers with its node; empty for an
    /// [`AppsWire::Present`], which is about every application.
    pub app: String,
    /// An [`UpdateMsg`] as the update lane encodes it, or [`WIRE_MAGIC`]
    /// followed by an [`AppsWire`] (bincode).
    pub payload: Vec<u8>,
}

/// What an [`AppsMsg`] holds, as this build reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppsContent {
    Update(UpdateMsg),
    Wire(AppsWire),
}

/// Messages between copies of one application, and what each machine runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppsWire {
    /// A message from a copy of the application on the sending machine to
    /// the copies here listening to `topic`; answered by [`Self::Delivered`]
    /// with the same `id`.
    Datagram {
        id: u64,
        from_process: Option<String>,
        topic: String,
        payload: Vec<u8>,
    },
    /// What became of datagram `id`.
    Delivered { id: u64, outcome: Delivery },
    /// Every application registered on the sending machine now, resent whole
    /// whenever that changes.
    Present { apps: Vec<AppPresence> },
    /// Which of these blobs (`sha1:<hex>`, …) the application's copy on the
    /// receiving machine holds; answered by [`Self::BlobHas`]. Added after
    /// the variants above, so their numbers stand.
    BlobHave { id: u64, hashes: Vec<String> },
    /// The answer to [`Self::BlobHave`] `id`: each held blob with its size,
    /// or why that machine gives none to the sender.
    BlobHas {
        id: u64,
        have: Vec<(String, u64)>,
        refused: Option<String>,
    },
    /// Part of a blob: `length` bytes from `offset`; answered by
    /// [`Self::BlobChunk`].
    BlobWant {
        id: u64,
        hash: String,
        offset: u64,
        length: u32,
    },
    /// The answer to [`Self::BlobWant`] `id`: the bytes, or why there are
    /// none.
    BlobChunk {
        id: u64,
        data: Vec<u8>,
        error: Option<String>,
    },
    /// Open stream `stream` to the DevTools endpoint the application's copy
    /// (`process` when named) declared on the receiving machine; answered by
    /// [`Self::TunnelOpened`]. Added after the variants above, so their
    /// numbers stand.
    TunnelOpen {
        stream: u64,
        process: Option<String>,
    },
    /// The answer to [`Self::TunnelOpen`]: the stream is open, or why not.
    TunnelOpened { stream: u64, error: Option<String> },
    /// Bytes of an open stream, either way.
    TunnelData { stream: u64, data: Vec<u8> },
    /// The receiving side wrote `bytes` more of the stream out: the sender
    /// may have that many more in flight.
    TunnelAck { stream: u64, bytes: u64 },
    /// The stream ended, from either side; why, when it was not just closed.
    TunnelClose { stream: u64, reason: Option<String> },
}

/// What became of a datagram on the receiving machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    /// Handed to this many running copies listening to its topic.
    Delivered { copies: u32 },
    /// No copy of the application there listens to that topic now.
    NotRunning,
    /// That machine's person does not let the sender reach it, or has not
    /// answered yet; why, in words.
    Refused(String),
}

/// One application registered on a machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppPresence {
    pub app: String,
    /// A copy of it is connected to its node now.
    pub running: bool,
    pub version: String,
    /// Topics its running copies listen to.
    pub topics: Vec<String>,
}

impl AppsMsg {
    pub fn new(app: &str, msg: &UpdateMsg) -> Self {
        AppsMsg {
            app: app.to_owned(),
            payload: msg.encode(),
        }
    }

    pub fn wire(app: &str, msg: &AppsWire) -> Self {
        let mut payload = WIRE_MAGIC.to_vec();
        payload.extend(bincode::serialize(msg).expect("apps messages always serialize"));
        AppsMsg {
            app: app.to_owned(),
            payload,
        }
    }

    /// What is inside, or `None` for a kind this build does not know.
    pub fn content(&self) -> Option<AppsContent> {
        match self.payload.strip_prefix(&WIRE_MAGIC) {
            Some(wire) => bincode::deserialize(wire).ok().map(AppsContent::Wire),
            None => UpdateMsg::decode(&self.payload)
                .ok()
                .flatten()
                .map(AppsContent::Update),
        }
    }

    /// The lane message inside, if it is one.
    pub fn update(&self) -> Option<UpdateMsg> {
        match self.content()? {
            AppsContent::Update(msg) => Some(msg),
            AppsContent::Wire(_) => None,
        }
    }

    /// A build's bytes, and a large message, are bulk, behind input and
    /// sound; an offer, a request or a small message is something the other
    /// side is waiting on.
    pub fn priority(&self) -> Priority {
        match self.content() {
            Some(AppsContent::Update(UpdateMsg::Chunk(_))) => Priority::Bulk,
            Some(AppsContent::Wire(AppsWire::Datagram { payload, .. }))
                if payload.len() > BULK_FROM =>
            {
                Priority::Bulk
            }
            Some(AppsContent::Wire(AppsWire::TunnelData { data, .. }))
                if data.len() > BULK_FROM =>
            {
                Priority::Bulk
            }
            Some(AppsContent::Wire(AppsWire::BlobChunk { .. })) => Priority::Bulk,
            _ => Priority::Metadata,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{Done, Want};

    #[test]
    fn a_lane_message_crosses_inside_its_application() {
        let want = UpdateMsg::Want(Want {
            version: "0.6.0+x".into(),
            platform: "windows-x86_64".into(),
            name: "leavepulse-launcher.exe".into(),
            offset: 0,
            length: 1024,
        });
        let msg = AppsMsg::new("leavepulse", &want);
        let bytes = bincode::serialize(&msg).unwrap();
        let back: AppsMsg = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.app, "leavepulse");
        assert_eq!(back.update(), Some(want));
        assert_eq!(back.priority(), Priority::Metadata);

        let done = UpdateMsg::Done(Done {
            version: "v".into(),
            platform: "windows-x86_64".into(),
            installed: true,
            reason: String::new(),
        });
        assert_eq!(AppsMsg::new("a", &done).update(), Some(done));
    }

    #[test]
    fn a_message_between_copies_rides_the_same_envelope_and_a_big_one_is_bulk() {
        let small = AppsWire::Datagram {
            id: 7,
            from_process: None,
            topic: "instances".into(),
            payload: b"hi".to_vec(),
        };
        let msg = AppsMsg::wire("leavepulse", &small);
        let back: AppsMsg = bincode::deserialize(&bincode::serialize(&msg).unwrap()).unwrap();
        assert_eq!(back.content(), Some(AppsContent::Wire(small)));
        assert_eq!(back.update(), None, "not a lane message");
        assert_eq!(back.priority(), Priority::Metadata);

        let big = AppsWire::Datagram {
            id: 8,
            from_process: Some("daemon".into()),
            topic: "sync".into(),
            payload: vec![0; BULK_FROM + 1],
        };
        assert_eq!(AppsMsg::wire("a", &big).priority(), Priority::Bulk);
    }

    #[test]
    fn a_blobs_bytes_ride_bulk_and_its_questions_do_not() {
        let chunk = AppsWire::BlobChunk {
            id: 1,
            data: vec![0; 64],
            error: None,
        };
        assert_eq!(AppsMsg::wire("a", &chunk).priority(), Priority::Bulk);
        let want = AppsWire::BlobWant {
            id: 2,
            hash: format!("sha1:{}", "a".repeat(40)),
            offset: 0,
            length: 1,
        };
        let msg = AppsMsg::wire("a", &want);
        assert_eq!(msg.priority(), Priority::Metadata);
        assert_eq!(msg.content(), Some(AppsContent::Wire(want)));
    }

    #[test]
    fn the_wire_kinds_before_blobs_keep_their_numbers() {
        // bincode numbers enum variants by position: the blob kinds were
        // added last so a node from before them reads every older kind.
        let present = bincode::serialize(&AppsWire::Present { apps: Vec::new() }).unwrap();
        assert_eq!(&present[..4], &[2, 0, 0, 0]);
        let have = bincode::serialize(&AppsWire::BlobHave {
            id: 0,
            hashes: Vec::new(),
        })
        .unwrap();
        assert_eq!(&have[..4], &[3, 0, 0, 0]);
    }

    #[test]
    fn a_node_that_predates_wire_messages_finds_nothing_it_knows_in_one() {
        // What an older build does with the envelope: decode the payload as
        // a lane message. It must find none, not a wrong one.
        let msg = AppsMsg::wire("a", &AppsWire::Present { apps: Vec::new() });
        assert!(UpdateMsg::decode(&msg.payload).ok().flatten().is_none());
    }

    #[test]
    fn the_envelope_keeps_the_bytes_old_nodes_read() {
        // The field was named `update`; bincode carries no names, so the
        // rename changes nothing on the wire.
        let msg = AppsMsg {
            app: "a".into(),
            payload: vec![1, 2],
        };
        assert_eq!(
            bincode::serialize(&msg).unwrap(),
            [1, 0, 0, 0, 0, 0, 0, 0, b'a', 2, 0, 0, 0, 0, 0, 0, 0, 1, 2]
        );
    }
}
