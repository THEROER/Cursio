//! Agreeing on a direct Wi-Fi link between two machines near each other.
//!
//! The session already runs through the router; this is the conversation, over
//! it, that puts a shorter path beside it. One machine runs a group, the other
//! joins it with a second station interface while both stay on their own
//! networks, and the mesh then adds paths over the group to the same session
//! (`docs/mesh-design.md`, "A direct Wi-Fi path").
//!
//! The group's passphrase crosses only here, inside the session's own Noise
//! channel, and is fresh for every group.

use serde::{Deserialize, Serialize};

use crate::Priority;

/// The direct module's messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectMsg {
    /// What the sender can do toward a direct link now. Sent when the session
    /// starts and whenever it changes; both ends derive who hosts from the
    /// pair of them alike.
    Able {
        host: bool,
        join: bool,
        /// The frequency the sender's own Wi-Fi is on, in MHz.
        freq_mhz: Option<u32>,
    },
    /// The host's group is up: join it.
    Group { ssid: String, passphrase: String },
    /// The joiner is on the group. Both addresses are on it: the host's is
    /// where the joiner dials, the joiner's is how the host knows the
    /// connection that arrives from it.
    Joined { host: [u8; 4], joiner: [u8; 4] },
    /// The sender left the link or stopped its group, and why.
    Left { why: String },
}

impl DirectMsg {
    /// Small, and wanted promptly: the link it sets up is what input will
    /// ride, but nothing here is input itself.
    pub fn priority(&self) -> Priority {
        Priority::Control
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_rides_the_control_class() {
        let msgs = [
            DirectMsg::Able {
                host: true,
                join: false,
                freq_mhz: Some(5745),
            },
            DirectMsg::Group {
                ssid: "DIRECT-ab-Cursio".into(),
                passphrase: "x".repeat(20),
            },
            DirectMsg::Joined {
                host: [192, 168, 137, 1],
                joiner: [192, 168, 137, 83],
            },
            DirectMsg::Left { why: "gone".into() },
        ];
        for msg in msgs {
            assert_eq!(msg.priority(), Priority::Control);
        }
    }
}
