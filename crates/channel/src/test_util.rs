//! A channel shaped like Cursio's — six classes on four lanes, with its
//! budgets — and a small envelope to drive it, so the scheduler and the
//! session are tested here against the declaration their first user ships.

use std::sync::Arc;
use std::time::Duration;

use crate::{ChannelError, Class, ClassId, Classes, Delivery, Envelope, Lane, LaneId};

pub(crate) const CONTROL: ClassId = 0;
pub(crate) const INPUT: ClassId = 1;
pub(crate) const AUDIO: ClassId = 2;
pub(crate) const VIDEO: ClassId = 3;
pub(crate) const METADATA: ClassId = 4;
pub(crate) const BULK: ClassId = 5;

pub(crate) const INTERACTIVE_LANE: LaneId = LaneId::new(0);
pub(crate) const AUDIO_LANE: LaneId = LaneId::new(1);
pub(crate) const VIDEO_LANE: LaneId = LaneId::new(2);
pub(crate) const BULK_LANE: LaneId = LaneId::new(3);

pub(crate) fn classes() -> Arc<Classes> {
    let lane = |name, path_name, writer_depth, paced, standby_skips_fresh| Lane {
        name,
        path_name,
        writer_depth,
        paced,
        standby_skips_fresh,
    };
    let class = |name, lane, budget, urgent| Class {
        name,
        lane,
        budget,
        urgent,
    };
    Arc::new(
        Classes::new(
            vec![
                lane("Interactive", "Network", 32, false, false),
                lane("Audio", "NetworkAudio", 2, false, false),
                lane("Video", "NetworkVideo", 2, false, true),
                lane("Bulk", "NetworkBulk", 2, true, false),
            ],
            vec![
                class("control", INTERACTIVE_LANE, 256 * 1024, true),
                class("input", INTERACTIVE_LANE, 256 * 1024, true),
                class("audio", AUDIO_LANE, 512 * 1024, false),
                class("video", VIDEO_LANE, 4 * 1024 * 1024, false),
                class("metadata", INTERACTIVE_LANE, 1024 * 1024, false),
                class("bulk", BULK_LANE, 8 * 1024 * 1024, false),
            ],
        )
        .unwrap()
        .with_standby_name("Bluetooth"),
    )
}

/// What the tests send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Msg {
    Key(u16),
    Motion(i32, i32),
    Click,
    Heartbeat(u64),
    /// Bytes in any class, reliable.
    Blob(ClassId, Vec<u8>),
    /// Sound: fresh for a quarter of a second.
    Sound(Vec<u8>),
    /// Pixels: fresh, never dropped for age.
    Pixels(Vec<u8>),
}

impl Envelope for Msg {
    fn class(&self) -> ClassId {
        match self {
            Msg::Key(_) | Msg::Motion(..) | Msg::Click => INPUT,
            Msg::Heartbeat(_) => CONTROL,
            Msg::Blob(class, _) => *class,
            Msg::Sound(_) => AUDIO,
            Msg::Pixels(_) => VIDEO,
        }
    }

    fn delivery(&self) -> Delivery {
        match self {
            Msg::Motion(..) | Msg::Pixels(_) => Delivery::Fresh { within: None },
            Msg::Sound(_) => Delivery::Fresh {
                within: Some(Duration::from_millis(250)),
            },
            _ => Delivery::Reliable,
        }
    }

    fn encode(&self) -> Result<Vec<u8>, ChannelError> {
        let mut out = Vec::new();
        match self {
            Msg::Key(k) => {
                out.push(0);
                out.extend_from_slice(&k.to_be_bytes());
            }
            Msg::Motion(x, y) => {
                out.push(1);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
            }
            Msg::Click => out.push(2),
            Msg::Heartbeat(n) => {
                out.push(3);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Msg::Blob(class, bytes) => {
                out.extend_from_slice(&[4, *class as u8]);
                out.extend_from_slice(bytes);
            }
            Msg::Sound(bytes) => {
                out.push(5);
                out.extend_from_slice(bytes);
            }
            Msg::Pixels(bytes) => {
                out.push(6);
                out.extend_from_slice(bytes);
            }
        }
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, ChannelError> {
        let bad = || ChannelError::Envelope(format!("bad test message {bytes:?}"));
        let (&tag, rest) = bytes.split_first().ok_or_else(bad)?;
        let int = |at: usize, len: usize| rest.get(at..at + len).ok_or_else(bad);
        Ok(match tag {
            0 => Msg::Key(u16::from_be_bytes(int(0, 2)?.try_into().unwrap())),
            1 => Msg::Motion(
                i32::from_be_bytes(int(0, 4)?.try_into().unwrap()),
                i32::from_be_bytes(int(4, 4)?.try_into().unwrap()),
            ),
            2 => Msg::Click,
            3 => Msg::Heartbeat(u64::from_be_bytes(int(0, 8)?.try_into().unwrap())),
            4 => Msg::Blob(
                usize::from(*rest.first().ok_or_else(bad)?),
                rest[1..].to_vec(),
            ),
            5 => Msg::Sound(rest.to_vec()),
            6 => Msg::Pixels(rest.to_vec()),
            _ => return Err(bad()),
        })
    }

    fn folds(&self) -> bool {
        matches!(self, Msg::Motion(..))
    }

    fn fold(&mut self, next: &Self) -> bool {
        let (Msg::Motion(x, y), Msg::Motion(dx, dy)) = (&*self, next) else {
            return false;
        };
        let (Some(x), Some(y)) = (x.checked_add(*dx), y.checked_add(*dy)) else {
            return false;
        };
        *self = Msg::Motion(x, y);
        true
    }

    fn is_activity(&self) -> bool {
        matches!(self, Msg::Key(_) | Msg::Motion(..) | Msg::Click)
    }
}

pub(crate) fn bulk(len: usize) -> Msg {
    Msg::Blob(BULK, vec![0; len])
}
