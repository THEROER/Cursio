//! Filesystem-sharing wire types.
//!
//! Each machine offers its volumes — `/` on Linux, `C:`, `D:`, … on Windows —
//! and the other mounts them as ordinary folders, so any application opens a
//! file across the link with no awareness that it is remote. See
//! `docs/files-design.md`.
//!
//! ## Request/response, not push
//!
//! Audio and clipboard push: the owner decides when something is worth sending.
//! A filesystem is the opposite — nothing should cross the link until a program
//! actually asks for it, because a volume is far larger than any link, and a
//! file manager merely *listing* a directory must not drag its contents over.
//! So every operation here is a request carrying a [`FileReqId`], answered by
//! exactly one reply quoting it. The mount side is a client; the owner never
//! speaks unprompted.
//!
//! The id also makes concurrency safe: several programs read different files at
//! once, replies interleave, and each lands where it belongs. Ids are per
//! session and per requester, and wrap — a `u64` at even a million operations a
//! second outlives any session.
//!
//! ## Chunked, because files are large
//!
//! [`FileRead`] names a byte range rather than "the file", and a large read
//! comes back as a sequence of [`FileData`] replies, for the same two reasons
//! the clipboard chunks: a payload can exceed
//! [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN), and an unbroken multi-gigabyte
//! transfer would monopolise the link that the shared cursor rides on. Writes
//! chunk symmetrically.
//!
//! Traffic here is [`Priority::Bulk`](crate::Priority::Bulk): copying a folder
//! must never be felt as mouse lag. That is a deliberate trade — a file copy
//! yields to input, audio and video, and is expected to.
//!
//! ## Paths on the wire
//!
//! Paths are volume-relative and always `/`-separated, never a native string:
//! `("C:", "Users/theroer/notes.txt")`. Windows and Linux disagree about
//! separators, case, drive letters and roots, so a native path would be
//! ambiguous the moment it crossed. The receiving side rebuilds a native path
//! against the volume's root, and — because a request arrives from another
//! machine — is responsible for verifying that the result stays inside that
//! volume ([`FileError::Denied`]).
//!
//! ## Errors are values
//!
//! Every failure is a [`FileError`] reply, never silence: a mounted filesystem
//! that stops answering hangs the caller in uninterruptible sleep, which is far
//! worse for the user than a clean `EIO`. The variants map onto the errnos a
//! FUSE or WinFsp layer must return, so the mount side never has to guess.

use serde::{Deserialize, Serialize};

/// Correlates one request with its reply. Unique per requester per session.
pub type FileReqId = u64;

/// An open file, as handed out by the owner in [`FileOpened`].
///
/// Opaque to the mount side: it indexes the owner's own table of open files, so
/// a handle from one session means nothing in another.
pub type FileHandle = u64;

/// One volume a machine offers.
///
/// `id` is what paths are addressed against and must be stable for the session;
/// `label` is for humans and may be anything (a Windows volume label, a Linux
/// mount point).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// Stable identifier used in paths: `"C:"`, `"D:"`, or `"/"`.
    pub id: String,
    /// Human-readable name for the mount: `"Windows (C:)"`, `"Root"`.
    pub label: String,
    /// Whether the owner will accept writes to this volume at all. A read-only
    /// volume rejects every mutating request with [`FileError::ReadOnly`], so
    /// the mount can present it read-only rather than failing at write time.
    pub writable: bool,
    /// Total size in bytes, and how much of it is free.
    ///
    /// Carried with the volume rather than fetched on demand because it is
    /// wanted on the very first `statfs` — which the file manager issues as soon
    /// as the folder is opened — and asking then would cost a round trip before
    /// anything is shown.
    ///
    /// Reporting these matters more than it looks. A filesystem that answers
    /// zero is not merely uninformative: applications compute "percent used"
    /// from these numbers, and a zero total makes that a division by zero —
    /// rclone's mount crashed callers with SIGFPE exactly this way. Others
    /// refuse to copy at all, seeing no free space. So an unknown capacity is
    /// reported as a plausible non-zero figure, never as nothing.
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// What a path is: the file kind plus the metadata a directory listing needs.
///
/// Deliberately small and platform-neutral. Ownership, ACLs and the rest do not
/// survive the crossing between Linux and Windows in any faithful way, so they
/// are not modelled: pretending to carry a uid that means nothing on the other
/// side is worse than admitting it does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// Path component this entry is for, relative to whatever was listed.
    pub name: String,
    /// Directory, regular file, or symlink.
    pub kind: FileKind,
    /// Size in bytes. Zero for directories.
    pub size: u64,
    /// Last modification, as seconds since the Unix epoch. Signed because
    /// Windows can hold timestamps before 1970 and clamping them to zero would
    /// make a file look modified at the epoch.
    pub modified_unix: i64,
    /// Whether the owner will accept writes to this specific path.
    pub writable: bool,
}

/// The three kinds of entry a listing distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
    /// A symlink. Not followed by the owner: resolving it there would let a link
    /// inside the volume point anywhere on the owner's disk, so the mount side
    /// decides what to do with it.
    Symlink,
}

/// How a file is being opened. Mirrors the flags a FUSE/WinFsp layer receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenMode {
    pub read: bool,
    pub write: bool,
    /// Create the file if it does not exist.
    pub create: bool,
    /// Truncate to zero length on open.
    pub truncate: bool,
    /// Fail if the file already exists (with `create`).
    pub exclusive: bool,
}

impl OpenMode {
    /// Read-only open — the common case, and the only one a read-only volume
    /// accepts.
    pub fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            create: false,
            truncate: false,
            exclusive: false,
        }
    }

    /// Whether this open would modify anything, and so must be refused on a
    /// read-only volume.
    pub fn is_mutating(&self) -> bool {
        self.write || self.create || self.truncate
    }
}

/// Why an operation failed.
///
/// Chosen to map cleanly onto both errno and NTSTATUS, because both mount
/// implementations must turn these back into platform errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileError {
    /// No such file or directory (`ENOENT`).
    NotFound,
    /// Permission denied by the owner's OS, or a path that escaped its volume
    /// (`EACCES`).
    Denied,
    /// The volume is shared read-only and the request would write (`EROFS`).
    ReadOnly,
    /// Operation needs a directory and got a file, or the reverse (`ENOTDIR` /
    /// `EISDIR`).
    WrongKind,
    /// Directory not empty (`ENOTEMPTY`).
    NotEmpty,
    /// The file already exists and the open was exclusive (`EEXIST`).
    Exists,
    /// Unknown volume id, or a handle the owner does not recognise (`EBADF`).
    BadHandle,
    /// Anything else, with the owner's message for the log (`EIO`).
    Io(String),
}

/// A block of file bytes on the wire, compressed only when that actually helps.
///
/// File data is not video: it must survive the crossing byte for byte, so the
/// only option is lossless compression, and how much that wins is decided
/// entirely by the data. Measured on this link (~17.5 MB/s over Wi-Fi):
///
/// | content              | compresses to | effective rate |
/// |----------------------|---------------|----------------|
/// | executables, DLLs    | ~38%          | ~46 MB/s       |
/// | source, text, logs   | ~30%          | ~58 MB/s       |
/// | mp4, jpg, zip        | 100%          | no change      |
///
/// So compression is worth roughly 3x on compressible data and nothing at all
/// on data that is already packed — which is most of what fills a disk. Hence
/// the enum rather than a flag: the sender tries, keeps the result only if it
/// genuinely shrank, and says which it did. zstd at level 1 runs at ~284 MB/s
/// here, an order of magnitude above the link, so the attempt never becomes the
/// bottleneck; on incompressible input it gives up at ~728 MB/s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chunk {
    /// Sent as-is: either it did not compress usefully, or compression is off.
    Raw(Vec<u8>),
    /// zstd-compressed, with the original length so the receiver can size its
    /// buffer in one allocation and reject a payload that decompresses to
    /// something other than what was promised.
    Zstd { data: Vec<u8>, plain_len: u32 },
}

impl Chunk {
    /// Below this ratio compression is not worth declaring.
    ///
    /// A chunk that only shrinks by a few percent costs both sides a
    /// compress/decompress pass to save a rounding error on a link that is
    /// already the slow part. 90% keeps the clear wins (text at 30%, binaries
    /// at 38%) and skips the noise.
    pub const WORTH_IT: f32 = 0.9;

    /// Bytes as they go on the wire, for accounting.
    pub fn wire_len(&self) -> usize {
        match self {
            Chunk::Raw(b) => b.len(),
            Chunk::Zstd { data, .. } => data.len(),
        }
    }

    /// Length after decompression — what the file actually contains.
    pub fn plain_len(&self) -> usize {
        match self {
            Chunk::Raw(b) => b.len(),
            Chunk::Zstd { plain_len, .. } => *plain_len as usize,
        }
    }

    /// Whether a compressed result is worth sending instead of the original.
    pub fn worth_compressing(plain: usize, compressed: usize) -> bool {
        compressed < (plain as f32 * Self::WORTH_IT) as usize
    }
}

/// Messages exchanged by the files module.
///
/// Roughly: the owner announces its volumes once; everything after that is the
/// mount side asking and the owner answering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FilesMsg {
    /// Owner -> peer: the volumes offered here. Sent on start, and again if the
    /// set changes (a USB disk appearing). Idempotent — the whole list, so a
    /// reconnect needs no delta.
    Volumes(Vec<Volume>),
    /// Mount -> owner: send me your volumes.
    ///
    /// A distinct message rather than an empty [`FilesMsg::Volumes`]. Overloading
    /// the announcement was a real bug: a peer asking for the list looked
    /// exactly like a peer announcing *no* volumes, so the request wiped the
    /// list it was trying to fetch and the mount was torn down a moment later.
    ListVolumes { req: FileReqId },

    /// Mount -> owner: list a directory.
    ReadDir {
        req: FileReqId,
        volume: String,
        path: String,
    },
    /// Owner -> mount: the entries, possibly in several replies.
    ///
    /// A directory with a hundred thousand files exceeds one frame, so listings
    /// stream: `last` marks the final batch.
    DirEntries {
        req: FileReqId,
        entries: Vec<FileMeta>,
        last: bool,
    },

    /// Mount -> owner: metadata for one path.
    Stat {
        req: FileReqId,
        volume: String,
        path: String,
    },
    /// Owner -> mount: the metadata.
    StatResult { req: FileReqId, meta: FileMeta },

    /// Mount -> owner: what does this symlink point at?
    ///
    /// The counterpart to [`FileKind::Symlink`]: a listing reports a link as a
    /// link without resolving it, which leaves the mount side holding something
    /// the kernel will ask it to read. Without this the answer is `ENOSYS`, and
    /// every symlink on the volume becomes an entry that can be neither read nor
    /// traversed — a directory walk trips over the same one repeatedly, paying a
    /// round trip each time.
    ReadLink {
        req: FileReqId,
        volume: String,
        path: String,
    },
    /// Owner -> mount: the link's target, exactly as stored.
    ///
    /// Unresolved and unvalidated, because only the mount side can judge it: a
    /// target is meaningful relative to the mounted tree, and one pointing
    /// outside the volume must arrive intact to be seen for what it is rather
    /// than silently reinterpreted against the owner's disk.
    LinkTarget { req: FileReqId, target: String },

    /// Mount -> owner: open a file, yielding a handle.
    Open {
        req: FileReqId,
        volume: String,
        path: String,
        mode: OpenMode,
    },
    /// Owner -> mount: the file is open.
    Opened {
        req: FileReqId,
        handle: FileHandle,
        size: u64,
    },
    /// Mount -> owner: done with a handle. Unanswered — there is nothing useful
    /// to say, and waiting for an ack would stall `close()` for a round trip.
    Close { handle: FileHandle },

    /// Mount -> owner: read `len` bytes at `offset`.
    Read {
        req: FileReqId,
        handle: FileHandle,
        offset: u64,
        len: u32,
    },
    /// Owner -> mount: bytes for a [`FilesMsg::Read`].
    ///
    /// A read may come back in several of these; `last` marks the end. Short
    /// data with `last` means end of file.
    Data {
        req: FileReqId,
        offset: u64,
        /// Payload, compressed or not — see [`Chunk`].
        bytes: Chunk,
        last: bool,
    },

    /// Mount -> owner: write bytes at `offset`.
    ///
    /// Large writes arrive as several messages sharing a `req`, each with its
    /// own offset; the owner replies once, after the one marked `last`.
    Write {
        req: FileReqId,
        handle: FileHandle,
        offset: u64,
        /// Payload, compressed or not — see [`Chunk`].
        bytes: Chunk,
        last: bool,
    },
    /// Owner -> mount: how many bytes were written.
    Wrote { req: FileReqId, written: u64 },

    /// Mount -> owner: create a directory.
    MkDir {
        req: FileReqId,
        volume: String,
        path: String,
    },
    /// Mount -> owner: remove a file or an empty directory.
    Remove {
        req: FileReqId,
        volume: String,
        path: String,
    },
    /// Mount -> owner: rename within one volume.
    Rename {
        req: FileReqId,
        volume: String,
        from: String,
        to: String,
    },
    /// Mount -> owner: set a file's length.
    Truncate {
        req: FileReqId,
        handle: FileHandle,
        size: u64,
    },
    /// Owner -> mount: a mutating request succeeded and has nothing to return.
    Ok { req: FileReqId },

    /// Owner -> mount: this request failed. Sent for every failure, so a caller
    /// never waits forever.
    Failed { req: FileReqId, error: FileError },
}

impl FilesMsg {
    /// Scheduling class for file traffic.
    ///
    /// `Bulk`, the lowest: copying a folder is throughput work with no deadline,
    /// while the cursor, audio and video on the same encrypted channel all have
    /// one. Anything higher would let a file copy be felt as input lag — the
    /// exact problem the priority scheduler exists to prevent.
    pub const PRIORITY: crate::Priority = crate::Priority::Bulk;

    /// Largest payload carried in one [`FilesMsg::Data`] or [`FilesMsg::Write`].
    ///
    /// Well under [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN) so a chunk plus its
    /// envelope always fits, and small enough that a big transfer yields to
    /// higher-priority traffic often rather than hogging the link between
    /// scheduling points.
    pub const MAX_CHUNK: usize = 256 * 1024;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk plus its envelope has to fit in one frame, or a read of exactly
    /// `MAX_CHUNK` bytes could never be delivered.
    #[test]
    fn a_full_chunk_fits_in_a_frame() {
        // A `const` block, so the two constants are checked against each other
        // when the crate is built rather than when someone runs the tests: a
        // chunk larger than a frame is not a failing test, it is a build that
        // should not have happened.
        const {
            assert!(
                FilesMsg::MAX_CHUNK < crate::MAX_FRAME_LEN,
                "a data chunk must leave room for its envelope"
            )
        };
        let msg = FilesMsg::Data {
            req: 1,
            offset: 0,
            // Raw is the worst case: a compressed chunk is smaller by
            // definition, so if this fits, every chunk fits.
            bytes: Chunk::Raw(vec![0u8; FilesMsg::MAX_CHUNK]),
            last: false,
        };
        let encoded = bincode::serialize(&msg).expect("serialises");
        assert!(
            encoded.len() < crate::MAX_FRAME_LEN,
            "a full chunk plus overhead ({}) must fit in a frame ({})",
            encoded.len(),
            crate::MAX_FRAME_LEN
        );
    }

    /// File traffic must never outrank the cursor.
    #[test]
    fn files_yield_to_input() {
        assert!(
            FilesMsg::PRIORITY < crate::Priority::Input,
            "bulk file transfer must not preempt input"
        );
        assert!(
            FilesMsg::PRIORITY < crate::Priority::Audio,
            "a file copy must not interrupt audio"
        );
    }

    /// Compression is declared only when it genuinely shrank the data.
    ///
    /// Most bytes on a disk are already packed — video, images, archives — and
    /// on those zstd returns roughly what it was given. Sending that back as
    /// "compressed" would cost both sides a decompress pass for nothing.
    #[test]
    fn compression_is_only_claimed_when_it_pays() {
        // Text-like: ~30% of the original. Clearly worth it.
        assert!(Chunk::worth_compressing(1000, 300));
        // Executable-like: ~38%. Worth it.
        assert!(Chunk::worth_compressing(1000, 380));
        // Already compressed: no real gain, and a small one is not worth a pass.
        assert!(!Chunk::worth_compressing(1000, 1000));
        assert!(!Chunk::worth_compressing(1000, 950));
        // Pathological: compression made it bigger.
        assert!(!Chunk::worth_compressing(1000, 1100));
    }

    /// Both forms report the file's real length, so the receiver can size a
    /// buffer without caring which one arrived.
    #[test]
    fn a_chunk_reports_its_plain_length_either_way() {
        let raw = Chunk::Raw(vec![7u8; 64]);
        assert_eq!(raw.plain_len(), 64);
        assert_eq!(raw.wire_len(), 64);

        let packed = Chunk::Zstd {
            data: vec![0u8; 20],
            plain_len: 64,
        };
        assert_eq!(packed.plain_len(), 64, "the file still holds 64 bytes");
        assert_eq!(packed.wire_len(), 20, "but only 20 crossed the link");
    }

    /// A read-only volume has to reject every mutating open, and only those.
    #[test]
    fn mutating_opens_are_recognised() {
        assert!(!OpenMode::read_only().is_mutating());
        for m in [
            OpenMode {
                write: true,
                ..OpenMode::read_only()
            },
            OpenMode {
                create: true,
                ..OpenMode::read_only()
            },
            OpenMode {
                truncate: true,
                ..OpenMode::read_only()
            },
        ] {
            assert!(
                m.is_mutating(),
                "{m:?} writes and must be refused read-only"
            );
        }
    }
}
