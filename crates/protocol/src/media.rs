//! What is playing on each machine, and the buttons to drive it from another.
//!
//! ## A list, sent when it changes
//!
//! Each side sends its whole list of players whenever something a remote
//! control would show changes: a track, a state, a capability. Not the
//! position: it moves every millisecond a track plays, and sending it would
//! turn an idle player into a stream. The position travels with the list as
//! "this far, at the moment the list was sent", and the receiver advances it
//! itself while [`PlaybackState::Playing`]; a seek is a change and resends it.
//!
//! ## Cover art is asked for
//!
//! Art is the only large thing here, most lists are never looked at closely,
//! and the same cover stays through a whole album. So a player names its art
//! by a hash ([`Player::art`]) and the receiver asks for bytes it has not seen
//! ([`MediaMsg::ArtRequest`]), already scaled down by the sender to at most
//! [`MAX_ART_BYTES`].

use serde::{Deserialize, Serialize};

/// A player's id on the machine that has it: stable while the player lives,
/// and meaningless anywhere else.
pub type PlayerId = String;

/// Largest cover sent. A notification's art is a few hundred pixels across;
/// anything bigger is a waste of a link a phone may be on over Bluetooth.
pub const MAX_ART_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

/// One player and what it is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Player {
    pub id: PlayerId,
    /// The application, as its own machine names it ("Spotify", "Firefox").
    pub app: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub state: PlaybackState,
    /// `None` for a stream or anything without an end.
    pub duration_ms: Option<u64>,
    /// How far in, when this list was sent.
    pub position_ms: Option<u64>,
    /// 0.0–1.0, when the player has a volume of its own.
    pub volume: Option<f32>,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    /// Hash of the cover, to ask for with [`MediaMsg::ArtRequest`].
    pub art: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MediaCommand {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    /// Jump to this position, in milliseconds from the start.
    Seek {
        position_ms: u64,
    },
    SetVolume {
        volume: f32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaMsg {
    /// Every player this side has, replacing the last list.
    Players(Vec<Player>),
    /// Press a button on one of the receiver's players.
    Command {
        player: PlayerId,
        command: MediaCommand,
    },
    /// Ask for the cover [`Player::art`] names.
    ArtRequest { player: PlayerId, art: u64 },
    /// The cover asked for, or nothing if it is gone or too large.
    Art {
        art: u64,
        mime: String,
        bytes: Vec<u8>,
    },
}

impl MediaMsg {
    /// A button press is felt, so it rides above bulk; a cover is a picture
    /// someone glances at, and a file copy may wait behind none of this but
    /// must not wait behind the cursor either.
    pub const PRIORITY: crate::Priority = crate::Priority::Video;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_player_list_round_trips() {
        let msg = MediaMsg::Players(vec![Player {
            id: "org.mpris.MediaPlayer2.spotify".into(),
            app: "Spotify".into(),
            title: "Song".into(),
            artist: "Band".into(),
            album: "Album".into(),
            state: PlaybackState::Playing,
            duration_ms: Some(180_000),
            position_ms: Some(12_000),
            volume: Some(0.5),
            can_play: true,
            can_pause: true,
            can_next: true,
            can_previous: true,
            can_seek: true,
            art: Some(42),
        }]);
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<MediaMsg>(&bytes).unwrap(), msg);
    }

    #[test]
    fn a_seek_command_carries_its_position() {
        let msg = MediaMsg::Command {
            player: "p".into(),
            command: MediaCommand::Seek {
                position_ms: 61_000,
            },
        };
        let bytes = bincode::serialize(&msg).unwrap();
        assert_eq!(bincode::deserialize::<MediaMsg>(&bytes).unwrap(), msg);
    }
}
