//! The update lane: the one vocabulary two builds of any age can always speak.
//!
//! Every other message on the wire is allowed to change shape, guarded by
//! [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION): a mismatch means the two
//! builds cannot hold a session. That rule has one bad consequence — the
//! machine that most needs a new build is exactly the one that can no longer
//! talk to the machine that has it. This vocabulary is how it still hears.
//!
//! ## Frozen, not versioned
//!
//! Nothing here is `serde`. Every message is laid out by hand, byte for byte,
//! and the tests pin those bytes: a change to any of them fails a test, which
//! is the point. What is frozen:
//!
//! - the envelope: [`MAGIC`], a lane byte that is always `1`, a kind byte;
//! - each kind's fields, in order, with their widths — integers big-endian,
//!   byte strings and UTF-8 strings as a `u32` length and the bytes;
//! - [`Message::Update`](crate::Message::Update) keeping its variant index
//!   (17) in [`Message`](crate::Message), and [`Message::Hello`](crate::Message::Hello)
//!   keeping index 0, since a peer that sent its old greeting is recognised by
//!   that alone.
//!
//! Growth is by **new kinds**, never by changing an old one: a receiver skips a
//! kind it does not know ([`UpdateMsg::decode`] answers `Ok(None)`), so a later
//! build can say more without an earlier one misreading it.
//!
//! ## Where it rides
//!
//! Every connection opens with a [`Preamble`] before anything else, so each side
//! learns the other's protocol version from a message that can never fail to
//! decode. Equal versions go on to the ordinary greeting, and the rest of this
//! vocabulary travels inside the session like any module's. Different versions
//! skip the greeting: the link is encrypted with the pair's stored key and
//! carries only this lane, long enough to hand over a build.

use crate::PeerId;

/// First bytes of every update-lane payload.
pub const MAGIC: [u8; 4] = *b"CUPD";

/// The lane's own format byte. Never changes; see the module header.
pub const LANE: u8 = 1;

const KIND_PREAMBLE: u8 = 1;
const KIND_OFFER: u8 = 2;
const KIND_WANT: u8 = 3;
const KIND_CHUNK: u8 = 4;
const KIND_DONE: u8 = 5;

/// First frame of every connection, sent before the greeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preamble {
    /// The sender's [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION).
    pub protocol_version: u16,
    pub peer_id: PeerId,
    /// What build the sender runs, e.g. `0.1.0+c9798e7060`.
    pub build: String,
    /// Which platform's executables it can run, e.g. `windows-x86_64`.
    pub platform: String,
}

/// One file of a build, as a signed [`Statement`] vouches for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub platform: String,
    /// File name inside that platform's build directory.
    pub name: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// Which release stream a build belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Signed with a release key compiled into every build.
    Stable,
    /// Signed with a builder key a user trusted on each receiving machine.
    Dev,
}

impl Channel {
    fn byte(self) -> u8 {
        match self {
            Channel::Stable => 0,
            Channel::Dev => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Channel::Stable),
            1 => Some(Channel::Dev),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Dev => "dev",
        }
    }
}

/// What a signature covers: one version, every platform's files.
///
/// Signed as its exact [`Statement::to_bytes`] encoding, which is frozen with
/// the rest of the lane — a verifier re-deriving the bytes differently would
/// refuse every genuine build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub version: String,
    pub channel: Channel,
    /// Seconds since the Unix epoch. Orders builds: version names carry a
    /// commit, and commits do not sort.
    pub published_at: u64,
    pub artifacts: Vec<Artifact>,
}

/// A build a node can hand over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// [`Statement::to_bytes`], kept as bytes so the signature is checked
    /// against exactly what was signed.
    pub statement: Vec<u8>,
    pub signer: [u8; 32],
    pub signature: [u8; 64],
    /// Platforms whose files the sender holds in full. Not signed: it is a
    /// claim about the sender's disk, and a false one only wastes a request.
    pub have: Vec<String>,
}

/// Ask for part of one file of an offered build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub version: String,
    pub platform: String,
    pub name: String,
    pub offset: u64,
    pub length: u32,
}

/// Part of a file, answering a [`Want`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub version: String,
    pub platform: String,
    pub name: String,
    pub offset: u64,
    pub data: Vec<u8>,
}

/// How a transfer ended on the receiving side, so the sender can stop and say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub version: String,
    pub platform: String,
    pub installed: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateMsg {
    Preamble(Preamble),
    Offer(Offer),
    Want(Want),
    Chunk(Chunk),
    Done(Done),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneError {
    #[error("not an update-lane payload")]
    NotLane,
    #[error("update-lane payload ends early")]
    Truncated,
    #[error("update-lane string is not utf-8")]
    BadString,
    #[error("unknown update channel {0}")]
    BadChannel(u8),
}

impl UpdateMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.raw(&MAGIC);
        w.u8(LANE);
        match self {
            UpdateMsg::Preamble(p) => {
                w.u8(KIND_PREAMBLE);
                w.u16(p.protocol_version);
                w.raw(p.peer_id.as_bytes());
                w.str(&p.build);
                w.str(&p.platform);
            }
            UpdateMsg::Offer(o) => {
                w.u8(KIND_OFFER);
                w.bytes(&o.statement);
                w.raw(&o.signer);
                w.raw(&o.signature);
                w.u32(o.have.len() as u32);
                for p in &o.have {
                    w.str(p);
                }
            }
            UpdateMsg::Want(q) => {
                w.u8(KIND_WANT);
                w.str(&q.version);
                w.str(&q.platform);
                w.str(&q.name);
                w.u64(q.offset);
                w.u32(q.length);
            }
            UpdateMsg::Chunk(c) => {
                w.u8(KIND_CHUNK);
                w.str(&c.version);
                w.str(&c.platform);
                w.str(&c.name);
                w.u64(c.offset);
                w.bytes(&c.data);
            }
            UpdateMsg::Done(d) => {
                w.u8(KIND_DONE);
                w.str(&d.version);
                w.str(&d.platform);
                w.u8(u8::from(d.installed));
                w.str(&d.reason);
            }
        }
        w.0
    }

    /// `Ok(None)` for a kind this build does not know: a later build may say
    /// more, and skipping it is what keeps the two able to talk.
    pub fn decode(bytes: &[u8]) -> Result<Option<Self>, LaneError> {
        let mut r = Reader(bytes);
        if r.take(4)? != MAGIC || r.u8()? != LANE {
            return Err(LaneError::NotLane);
        }
        let msg = match r.u8()? {
            KIND_PREAMBLE => UpdateMsg::Preamble(Preamble {
                protocol_version: r.u16()?,
                peer_id: PeerId::from_bytes(r.array()?),
                build: r.str()?,
                platform: r.str()?,
            }),
            KIND_OFFER => {
                let statement = r.bytes()?;
                let signer = r.array()?;
                let signature = r.array()?;
                let count = r.u32()?;
                let mut have = Vec::new();
                for _ in 0..count {
                    have.push(r.str()?);
                }
                UpdateMsg::Offer(Offer {
                    statement,
                    signer,
                    signature,
                    have,
                })
            }
            KIND_WANT => UpdateMsg::Want(Want {
                version: r.str()?,
                platform: r.str()?,
                name: r.str()?,
                offset: r.u64()?,
                length: r.u32()?,
            }),
            KIND_CHUNK => UpdateMsg::Chunk(Chunk {
                version: r.str()?,
                platform: r.str()?,
                name: r.str()?,
                offset: r.u64()?,
                data: r.bytes()?,
            }),
            KIND_DONE => UpdateMsg::Done(Done {
                version: r.str()?,
                platform: r.str()?,
                installed: r.u8()? != 0,
                reason: r.str()?,
            }),
            _ => return Ok(None),
        };
        Ok(Some(msg))
    }
}

impl Statement {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.str(&self.version);
        w.u8(self.channel.byte());
        w.u64(self.published_at);
        w.u32(self.artifacts.len() as u32);
        for a in &self.artifacts {
            w.str(&a.platform);
            w.str(&a.name);
            w.u64(a.size);
            w.raw(&a.sha256);
        }
        w.0
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LaneError> {
        let mut r = Reader(bytes);
        let version = r.str()?;
        let channel = r.u8()?;
        let channel = Channel::from_byte(channel).ok_or(LaneError::BadChannel(channel))?;
        let published_at = r.u64()?;
        let count = r.u32()?;
        let mut artifacts = Vec::new();
        for _ in 0..count {
            artifacts.push(Artifact {
                platform: r.str()?,
                name: r.str()?,
                size: r.u64()?,
                sha256: r.array()?,
            });
        }
        Ok(Self {
            version,
            channel,
            published_at,
            artifacts,
        })
    }

    /// The files one platform needs, in the order they were signed.
    pub fn for_platform<'a>(&'a self, platform: &'a str) -> impl Iterator<Item = &'a Artifact> {
        self.artifacts
            .iter()
            .filter(move |a| a.platform == platform)
    }
}

/// This build's platform name, as a [`Preamble`] and a [`Statement`] spell it.
pub fn this_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.raw(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.raw(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.raw(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.raw(b);
    }
    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], LaneError> {
        if self.0.len() < n {
            return Err(LaneError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], LaneError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, LaneError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, LaneError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, LaneError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, LaneError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, LaneError> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Result<String, LaneError> {
        String::from_utf8(self.bytes()?).map_err(|_| LaneError::BadString)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn the_preamble_encoding_is_frozen() {
        let msg = UpdateMsg::Preamble(Preamble {
            protocol_version: 19,
            peer_id: PeerId::from_bytes([7; 16]),
            build: "b".into(),
            platform: "p".into(),
        });
        assert_eq!(
            hex(&msg.encode()),
            "43555044010100130707070707070707070707070707070700000001620000000170"
        );
    }

    #[test]
    fn the_want_chunk_and_done_encodings_are_frozen() {
        let want = UpdateMsg::Want(Want {
            version: "v".into(),
            platform: "p".into(),
            name: "n".into(),
            offset: 2,
            length: 3,
        });
        assert_eq!(
            hex(&want.encode()),
            "4355504401030000000176000000017000000001 6e000000000000000200000003".replace(' ', "")
        );
        let chunk = UpdateMsg::Chunk(Chunk {
            version: "v".into(),
            platform: "p".into(),
            name: "n".into(),
            offset: 1,
            data: vec![0xaa],
        });
        assert_eq!(
            hex(&chunk.encode()),
            "43555044010400000001760000000170000000016e000000000000000100000001aa"
        );
        let done = UpdateMsg::Done(Done {
            version: "v".into(),
            platform: "p".into(),
            installed: true,
            reason: String::new(),
        });
        assert_eq!(
            hex(&done.encode()),
            "4355504401050000000176000000017001 00000000".replace(' ', "")
        );
    }

    #[test]
    fn the_signed_statement_encoding_is_frozen() {
        let statement = Statement {
            version: "v".into(),
            channel: Channel::Dev,
            published_at: 1,
            artifacts: vec![Artifact {
                platform: "p".into(),
                name: "n".into(),
                size: 4,
                sha256: [1; 32],
            }],
        };
        assert_eq!(
            hex(&statement.to_bytes()),
            format!(
                "000000017601000000000000000100000001000000017000000001 6e0000000000000004{}",
                "01".repeat(32)
            )
            .replace(' ', "")
        );
        assert_eq!(
            Statement::from_bytes(&statement.to_bytes()).unwrap(),
            statement
        );
    }

    #[test]
    fn every_kind_survives_a_round_trip() {
        let offer = UpdateMsg::Offer(Offer {
            statement: vec![1, 2, 3],
            signer: [2; 32],
            signature: [3; 64],
            have: vec!["linux-x86_64".into(), "windows-x86_64".into()],
        });
        assert_eq!(UpdateMsg::decode(&offer.encode()).unwrap(), Some(offer));
    }

    #[test]
    fn a_kind_from_a_later_build_is_skipped_not_misread() {
        let later = [MAGIC.as_slice(), &[LANE, 200, 1, 2, 3]].concat();
        assert_eq!(UpdateMsg::decode(&later), Ok(None));
    }

    #[test]
    fn foreign_and_truncated_payloads_are_errors() {
        assert_eq!(UpdateMsg::decode(b"nope!!"), Err(LaneError::NotLane));
        let mut cut = UpdateMsg::Want(Want {
            version: "v".into(),
            platform: "p".into(),
            name: "n".into(),
            offset: 0,
            length: 1,
        })
        .encode();
        cut.truncate(cut.len() - 2);
        assert_eq!(UpdateMsg::decode(&cut), Err(LaneError::Truncated));
    }
}
