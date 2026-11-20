//! Which drive letter a peer's volume gets on Windows, remembered across
//! mounts.
//!
//! The letter used to be simply the first free one counting down from `Z:`,
//! so it depended on the order the volumes happened to mount in: a peer's
//! `/mnt/LocalDrive500` came up on `U:` one day and `X:` the next, its `/` on
//! `Y:` or `Z:`, a phone's storage on whichever was left. Anything that stores
//! a drive-letter path — a Steam library, a recent-files list, a shortcut —
//! then points at the wrong disk. Seen live: Steam on Windows kept asking the
//! peer for `//SteamLibrary` on a volume that had never held it, some 180 000
//! failing `stat`/`mkdir` requests a day.
//!
//! So each volume is remembered by **peer id and volume id** — never by the
//! peer's display name, which the user may change, nor by the order the peer
//! lists its volumes in — and gets the same letter every time it is free.
//!
//! What it does not do is take a letter by force. A remembered letter that
//! something else now holds (a USB stick, a `subst`, another of the peer's
//! volumes) is left alone: the volume mounts elsewhere for now, the reason is
//! logged, and the record is *not* moved, so the volume returns to its letter
//! once that is free again. Moving the record would let one stray mount order
//! permanently reshuffle every drive, which is the bug this exists to fix.
//!
//! A letter freed by a peer that left stays reserved for it: a volume with no
//! letter yet is given one nobody remembers, and takes a remembered one only
//! when nothing else is free — a drive that cannot mount is worse than one
//! that moved. Two records may then name the same letter; whichever volume
//! mounts first holds it and the other falls back, as above.
//!
//! The record is a small JSON file in the node's data directory, passed in by
//! the node rather than derived here: on Windows the mount runs in the separate
//! `cursio-mount-helper` process, which must use the same file as a node that
//! mounts for itself. It is read afresh for every mount for the same reason.
//! A missing or unreadable file only costs the memory, never the mount.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Letters a peer's volume may take, in the order a fresh one is tried.
///
/// Downwards from `Z:` because the low letters belong to real hardware and to
/// whatever the user maps by habit; a share from another machine belongs at
/// the far end, which is also where Windows puts mapped network drives. `A:`
/// to `C:` are never offered.
fn candidates() -> impl Iterator<Item = char> {
    ('D'..='Z').rev()
}

/// Bit for `letter` in a `GetLogicalDrives` mask: bit 0 is `A:`.
fn bit(letter: char) -> u32 {
    1 << (letter as u8 - b'A')
}

fn is_candidate(letter: char) -> bool {
    ('D'..='Z').contains(&letter)
}

/// Records kept beyond this are dropped, least recently mounted first. There
/// are only 23 letters to hand out; the bound keeps a long history of sticks
/// and forgotten peers from growing the file without limit.
const MAX_RECORDS: usize = 128;

/// One peer volume, as the record knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeKey {
    /// The peer's stable id, not its display name.
    pub peer: String,
    /// The volume's wire id (`"C:"`, `"/"`, a removable medium's mount path).
    pub volume: String,
}

/// The letter a mount should use, and how it came to be chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// The volume's own remembered letter, free.
    Remembered(char),
    /// A volume with no letter yet; this one becomes its letter.
    Fresh(char),
    /// The remembered letter is held by something else; `letter` is used for
    /// now and the record keeps `remembered`.
    Elsewhere { letter: char, remembered: char },
}

impl Pick {
    pub fn letter(self) -> char {
        match self {
            Pick::Remembered(c) | Pick::Fresh(c) => c,
            Pick::Elsewhere { letter, .. } => letter,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    peer: String,
    volume: String,
    letter: char,
    /// Unix seconds of the last mount on this letter; orders eviction.
    last_mounted: u64,
}

/// Every remembered assignment on this machine.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LetterBook {
    records: Vec<Record>,
}

impl LetterBook {
    /// Read the record at `path`. Absent is an empty book; unreadable or
    /// malformed is an empty book and a warning, since refusing to mount over
    /// a damaged memory would be the worse failure.
    pub fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read the remembered drive letters; choosing afresh");
                return Self::default();
            }
        };
        match serde_json::from_slice::<LetterBook>(&bytes) {
            Ok(mut book) => {
                book.records.retain(|r| is_candidate(r.letter));
                book
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "the remembered drive letters are malformed; choosing afresh");
                Self::default()
            }
        }
    }

    /// Write the book to `path`, through a temporary file and a rename so a
    /// crash mid-write leaves the previous record rather than half of one.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let tmp = tmp_path(path);
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    fn remembered(&self, key: &VolumeKey) -> Option<char> {
        self.records
            .iter()
            .find(|r| r.peer == key.peer && r.volume == key.volume)
            .map(|r| r.letter)
    }

    /// The letter for `key`, given the letters in use now (`used`, a
    /// `GetLogicalDrives` mask). `None` when every candidate is taken.
    pub fn choose(&self, key: &VolumeKey, used: u32) -> Option<Pick> {
        let remembered = self.remembered(key);
        if let Some(letter) = remembered {
            if used & bit(letter) == 0 {
                return Some(Pick::Remembered(letter));
            }
        }
        let reserved = |c: char| {
            self.records
                .iter()
                .any(|r| r.letter == c && !(r.peer == key.peer && r.volume == key.volume))
        };
        let free = |c: &char| used & bit(*c) == 0;
        let letter = candidates()
            .filter(free)
            .find(|c| !reserved(*c))
            .or_else(|| candidates().find(free))?;
        Some(match remembered {
            Some(remembered) => Pick::Elsewhere { letter, remembered },
            None => Pick::Fresh(letter),
        })
    }

    /// Note that `key` was mounted as `pick` said, at `now` (Unix seconds).
    ///
    /// Only a fresh letter becomes the volume's; a fallback leaves the
    /// remembered one in place for next time.
    pub fn record(&mut self, key: &VolumeKey, pick: Pick, now: u64) {
        match pick {
            Pick::Remembered(_) => {
                if let Some(r) = self
                    .records
                    .iter_mut()
                    .find(|r| r.peer == key.peer && r.volume == key.volume)
                {
                    r.last_mounted = now;
                }
            }
            Pick::Fresh(letter) => {
                self.records
                    .retain(|r| !(r.peer == key.peer && r.volume == key.volume));
                self.records.push(Record {
                    peer: key.peer.clone(),
                    volume: key.volume.clone(),
                    letter,
                    last_mounted: now,
                });
                if self.records.len() > MAX_RECORDS {
                    self.records
                        .sort_by_key(|r| std::cmp::Reverse(r.last_mounted));
                    self.records.truncate(MAX_RECORDS);
                }
            }
            Pick::Elsewhere { .. } => {}
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(peer: &str, volume: &str) -> VolumeKey {
        VolumeKey {
            peer: peer.into(),
            volume: volume.into(),
        }
    }

    fn mask(letters: &str) -> u32 {
        letters.chars().map(bit).fold(0, |m, b| m | b)
    }

    /// Mount `keys` in order against `used`, recording each, and say where
    /// each went.
    fn mount_all(book: &mut LetterBook, keys: &[VolumeKey], mut used: u32) -> Vec<char> {
        keys.iter()
            .map(|k| {
                let pick = book.choose(k, used).expect("a free letter");
                book.record(k, pick, 1);
                used |= bit(pick.letter());
                pick.letter()
            })
            .collect()
    }

    /// Mounting onto a letter already in use fails outright, so the mask must
    /// be read as `GetLogicalDrives` writes it: bit 0 is `A:`.
    #[test]
    fn letters_in_use_are_read_from_the_logical_drives_mask() {
        let book = LetterBook::default();
        let a_and_c = 0b101;
        assert_eq!(mask("AC"), a_and_c);
        assert_eq!(book.choose(&key("p", "/"), a_and_c), Some(Pick::Fresh('Z')));
        let all_but_z = !(1u32 << (b'Z' - b'A'));
        assert_eq!(
            book.choose(&key("p", "/"), all_but_z),
            Some(Pick::Fresh('Z'))
        );
        let all_but_d = !(1u32 << (b'D' - b'A'));
        assert_eq!(
            book.choose(&key("p", "/"), all_but_d),
            Some(Pick::Fresh('D'))
        );
    }

    #[test]
    fn a_volume_keeps_its_letter_whatever_order_the_volumes_mount_in() {
        let nocturne_root = key("nocturne-id", "/");
        let nocturne_data = key("nocturne-id", "/mnt/LocalDrive500");
        let phone = key("poco-id", "/storage/emulated/0");
        let mut book = LetterBook::default();
        let first = mount_all(
            &mut book,
            &[nocturne_root.clone(), nocturne_data.clone(), phone.clone()],
            mask("C"),
        );
        let reversed = mount_all(&mut book, &[phone, nocturne_data, nocturne_root], mask("C"));
        assert_eq!(first, vec!['Z', 'Y', 'X']);
        assert_eq!(reversed, vec!['X', 'Y', 'Z']);
    }

    #[test]
    fn a_fresh_volume_skips_letters_remembered_for_a_peer_that_left() {
        let mut book = LetterBook::default();
        mount_all(&mut book, &[key("gone", "/")], mask("C"));
        // The peer that held Z: is offline; its letter is free but stays its.
        assert_eq!(
            book.choose(&key("new", "/"), mask("C")),
            Some(Pick::Fresh('Y'))
        );
    }

    #[test]
    fn a_fresh_volume_takes_a_reserved_letter_only_when_nothing_else_is_free() {
        let mut book = LetterBook::default();
        mount_all(&mut book, &[key("gone", "/")], mask("C"));
        let all_but_z = mask("DEFGHIJKLMNOPQRSTUVWXY");
        assert_eq!(
            book.choose(&key("new", "/"), all_but_z),
            Some(Pick::Fresh('Z'))
        );
    }

    #[test]
    fn a_remembered_letter_held_by_something_else_is_not_taken_or_forgotten() {
        let data = key("nocturne-id", "/mnt/LocalDrive500");
        let mut book = LetterBook::default();
        mount_all(&mut book, std::slice::from_ref(&data), mask("C"));

        // A stick now sits on Z:; the volume mounts elsewhere for now…
        let pick = book.choose(&data, mask("CZ")).unwrap();
        assert_eq!(
            pick,
            Pick::Elsewhere {
                letter: 'Y',
                remembered: 'Z'
            }
        );
        book.record(&data, pick, 2);

        // …and goes back to Z: once the stick is gone.
        assert_eq!(book.choose(&data, mask("C")), Some(Pick::Remembered('Z')));
    }

    #[test]
    fn two_volumes_remembering_one_letter_go_to_whichever_mounts_first() {
        let a = key("a", "/");
        let b = key("b", "/");
        let mut book = LetterBook::default();
        mount_all(&mut book, std::slice::from_ref(&a), mask("C"));
        // `b` arrives while every other letter is taken, and shares Z:'s record.
        let crowded = mask("DEFGHIJKLMNOPQRSTUVWXY");
        let pick = book.choose(&b, crowded).unwrap();
        book.record(&b, pick, 2);
        assert_eq!(pick, Pick::Fresh('Z'));

        // Both now remember Z:. With `b` holding it, `a` falls back, keeping
        // its record; the reverse holds too.
        assert!(matches!(
            book.choose(&a, mask("CZ")),
            Some(Pick::Elsewhere {
                remembered: 'Z',
                ..
            })
        ));
        assert_eq!(book.choose(&b, mask("C")), Some(Pick::Remembered('Z')));
    }

    #[test]
    fn no_free_letter_is_none_rather_than_a_collision() {
        let book = LetterBook::default();
        assert_eq!(book.choose(&key("p", "/"), u32::MAX), None);
        // C: is never offered, even when free.
        assert_eq!(book.choose(&key("p", "/"), !mask("C")), None);
    }

    #[test]
    fn the_same_volume_name_on_two_peers_is_two_volumes() {
        let mut book = LetterBook::default();
        let letters = mount_all(&mut book, &[key("a", "/"), key("b", "/")], mask("C"));
        assert_eq!(letters, vec!['Z', 'Y']);
    }

    #[test]
    fn the_book_survives_a_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("drive-letters.json");
        let mut book = LetterBook::default();
        mount_all(&mut book, &[key("a", "/"), key("b", "C:")], mask("C"));
        book.save(&path).unwrap();
        assert_eq!(LetterBook::load(&path), book);
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn a_missing_or_damaged_book_is_empty_and_letters_outside_d_to_z_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("drive-letters.json");
        assert_eq!(LetterBook::load(&path), LetterBook::default());

        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(LetterBook::load(&path), LetterBook::default());

        std::fs::write(
            &path,
            br#"{"records":[{"peer":"p","volume":"/","letter":"C","last_mounted":1}]}"#,
        )
        .unwrap();
        assert_eq!(
            LetterBook::load(&path).choose(&key("p", "/"), mask("")),
            Some(Pick::Fresh('Z'))
        );
    }

    #[test]
    fn the_book_forgets_the_least_recently_mounted_beyond_its_bound() {
        let mut book = LetterBook::default();
        for i in 0..=MAX_RECORDS as u64 {
            book.record(&key(&format!("p{i}"), "/"), Pick::Fresh('Z'), i);
        }
        assert_eq!(book.records.len(), MAX_RECORDS);
        assert!(book.remembered(&key("p0", "/")).is_none());
        assert!(book
            .remembered(&key(&format!("p{MAX_RECORDS}"), "/"))
            .is_some());
    }
}
