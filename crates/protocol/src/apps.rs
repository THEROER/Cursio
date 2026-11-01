//! Builds of other applications, carried between machines the way Cursio's
//! own are (`docs/app-sdk-design.md`).
//!
//! The update lane's messages are reused byte for byte — offer, want, chunk,
//! done — each wrapped with the application it is about. The update lane
//! itself is not: its bytes are frozen so that two Cursio builds of any age
//! can still update each other, and a second meaning in it would be a second
//! thing every old build has to understand. Here a build that does not know
//! this module simply does not advertise it.

use serde::{Deserialize, Serialize};

use crate::update::UpdateMsg;
use crate::Priority;

/// One update-lane message about one application's builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsMsg {
    /// The application's id, as it registers with its node.
    pub app: String,
    /// An [`UpdateMsg`], encoded as the update lane encodes it.
    pub update: Vec<u8>,
}

impl AppsMsg {
    pub fn new(app: &str, msg: &UpdateMsg) -> Self {
        AppsMsg {
            app: app.to_owned(),
            update: msg.encode(),
        }
    }

    /// The lane message inside, or `None` for a kind this build does not
    /// know.
    pub fn update(&self) -> Option<UpdateMsg> {
        UpdateMsg::decode(&self.update).ok().flatten()
    }

    /// A build's bytes are bulk, behind input and sound; an offer or a
    /// request is a few bytes the other side is waiting on.
    pub fn priority(&self) -> Priority {
        match self.update() {
            Some(UpdateMsg::Chunk(_)) => Priority::Bulk,
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
}
