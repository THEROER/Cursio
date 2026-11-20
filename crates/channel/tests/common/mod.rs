//! What the trust tests share: a panel's messages on two lanes, and an
//! issuer whose proofs are plain text, so a test can write any proof it
//! wants and revoke any serial.

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use cursio_channel::mem::MemCarrier;
use cursio_channel::{
    Carrier, ChannelError, Class, ClassId, Classes, Delivery, Envelope, Lane, LaneId, NoisePsk,
    Opened, Path, PathKind, PeerIdentity, Revoked, Role, TlsInfo, Trust, Verified,
};

pub const CONSOLE: ClassId = 0;
pub const FILES: ClassId = 1;
pub const BULK: LaneId = LaneId::new(1);

pub fn classes() -> Arc<Classes> {
    let lane = |name, paced| Lane {
        name,
        path_name: name,
        writer_depth: 8,
        paced,
        standby_skips_fresh: false,
    };
    let class = |name, lane| Class {
        name,
        lane,
        budget: 1024 * 1024,
        urgent: false,
    };
    Arc::new(
        Classes::new(
            vec![lane("console", false), lane("bulk", true)],
            vec![class("console", LaneId::INTERACTIVE), class("files", BULK)],
        )
        .unwrap(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Panel {
    /// A line of a console: reliable, and past `expires_at` worth nothing.
    Console(String, Option<SystemTime>),
    Chunk(u32, Vec<u8>),
}

impl Envelope for Panel {
    fn class(&self) -> ClassId {
        match self {
            Panel::Console(..) => CONSOLE,
            Panel::Chunk(..) => FILES,
        }
    }

    fn delivery(&self) -> Delivery {
        Delivery::Reliable
    }

    fn encode(&self) -> Result<Vec<u8>, ChannelError> {
        let mut out = Vec::new();
        match self {
            Panel::Console(text, at) => {
                out.push(b'c');
                let millis = at.map_or(0, |at| {
                    at.duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64
                });
                out.extend_from_slice(&millis.to_be_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            Panel::Chunk(n, bytes) => {
                out.push(b'f');
                out.extend_from_slice(&n.to_be_bytes());
                out.extend_from_slice(bytes);
            }
        }
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, ChannelError> {
        let bad = || ChannelError::Envelope("not a panel message".into());
        let (&tag, rest) = bytes.split_first().ok_or_else(bad)?;
        match tag {
            b'c' if rest.len() >= 8 => {
                let millis = u64::from_be_bytes(rest[..8].try_into().unwrap());
                let at =
                    (millis != 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_millis(millis));
                let text = String::from_utf8(rest[8..].to_vec()).map_err(|_| bad())?;
                Ok(Panel::Console(text, at))
            }
            b'f' if rest.len() >= 4 => Ok(Panel::Chunk(
                u32::from_be_bytes(rest[..4].try_into().unwrap()),
                rest[4..].to_vec(),
            )),
            _ => Err(bad()),
        }
    }
}

pub fn line(text: &str) -> Panel {
    Panel::Console(text.into(), None)
}

/// Proofs as text, `id|serial|unix millis`, and a deny list of serials.
#[derive(Default)]
pub struct Issuer {
    deny: Mutex<HashSet<String>>,
}

impl Issuer {
    pub fn deny(&self, serial: &str) {
        self.deny.lock().unwrap().insert(serial.into());
    }

    pub fn proof(id: &str, serial: &str, lasts: Duration) -> Verified {
        Verified {
            identity: PeerIdentity::Host { id: id.into() },
            serial: Some(serial.into()),
            not_after: Some(SystemTime::now() + lasts),
        }
    }

    pub fn write(proof: &Verified) -> Vec<u8> {
        let PeerIdentity::Host { id } = &proof.identity else {
            panic!("the test issuer proves hosts only");
        };
        let millis = proof
            .not_after
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        format!("{id}|{}|{millis}", proof.serial.as_deref().unwrap()).into_bytes()
    }
}

#[async_trait]
impl Trust for Issuer {
    async fn open(&self, _: &mut dyn Carrier, _: Role) -> Result<Opened, ChannelError> {
        Err(ChannelError::Other(
            "the issuer opens nothing itself".into(),
        ))
    }

    fn check(&self, peer: &Verified, _: SystemTime) -> Result<(), Revoked> {
        match &peer.serial {
            Some(serial) if self.deny.lock().unwrap().contains(serial) => {
                Err(Revoked(format!("serial {serial} is on the deny list")))
            }
            _ => Ok(()),
        }
    }

    async fn reverify(&self, proof: &[u8], _: Option<&TlsInfo>) -> Result<Verified, ChannelError> {
        let text =
            String::from_utf8(proof.to_vec()).map_err(|e| ChannelError::Other(e.to_string()))?;
        let mut parts = text.split('|');
        let (Some(id), Some(serial), Some(millis)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(ChannelError::Other("a malformed proof".into()));
        };
        let millis: u64 = millis
            .parse()
            .map_err(|_| ChannelError::Other("a bad time".into()))?;
        Ok(Verified {
            identity: PeerIdentity::Host { id: id.into() },
            serial: Some(serial.into()),
            not_after: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(millis)),
        })
    }
}

/// One connection between the two ends, each having proved to the other
/// what the test says: `a_sees` is what `a`'s path proved of `b`.
pub async fn path_pair(a_sees: Verified, b_sees: Verified) -> (Path, Path, [u8; 16]) {
    let (mut a, mut b) = MemCarrier::pair();
    let trust = NoisePsk::from_secret(b"panel", b"a secret the two ends share");
    let (oa, ob) = tokio::join!(
        trust.open(&mut a, Role::Initiator),
        trust.open(&mut b, Role::Responder)
    );
    let (oa, ob) = (oa.unwrap(), ob.unwrap());
    let tag = oa.session_tag;
    let (a, b): (Box<dyn Carrier>, Box<dyn Carrier>) = (Box::new(a), Box::new(b));
    (
        Path::new(a, oa.seal, PathKind::INTERACTIVE, false).proving(a_sees),
        Path::new(b, ob.seal, PathKind::INTERACTIVE, false).proving(b_sees),
        tag,
    )
}
