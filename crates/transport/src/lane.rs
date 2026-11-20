//! Cursio's classes and lanes, declared for the channel.
//!
//! Six priority classes on four independent lanes (`docs/transport-design.md`):
//! session control, input and metadata share the interactive lane; sound,
//! pixels and bulk each have one of their own, so a file transfer stuck
//! behind a full socket cannot hold up the cursor. The budgets are the
//! scheduler's per-class byte limits.

use std::sync::{Arc, OnceLock};

use cursio_channel::{Class, Classes, LaneId};
use cursio_protocol::Priority;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Lane {
    Interactive = 0,
    Audio = 1,
    Video = 2,
    Bulk = 3,
}

impl Lane {
    pub const ALL: [Self; 4] = [Self::Interactive, Self::Audio, Self::Video, Self::Bulk];
    pub const COUNT: usize = Self::ALL.len();

    pub fn for_priority(priority: Priority) -> Self {
        match priority {
            Priority::Control | Priority::Input | Priority::Metadata => Self::Interactive,
            Priority::Audio => Self::Audio,
            Priority::Video => Self::Video,
            Priority::Bulk => Self::Bulk,
        }
    }
}

impl From<Lane> for LaneId {
    fn from(lane: Lane) -> Self {
        LaneId::new(lane as u8)
    }
}

/// The channel Cursio's sessions are built from, one declaration shared by
/// every link.
pub fn classes() -> Arc<Classes> {
    static CLASSES: OnceLock<Arc<Classes>> = OnceLock::new();
    CLASSES.get_or_init(declare).clone()
}

fn declare() -> Arc<Classes> {
    let lane = |name, path_name, writer_depth, paced, standby_skips_fresh| cursio_channel::Lane {
        name,
        path_name,
        writer_depth,
        paced,
        standby_skips_fresh,
    };
    let lanes = vec![
        // Two frames for the lanes whose frames are large, so their order
        // stays with the scheduler rather than in a queue it cannot see. The
        // interactive lane's are tens of bytes, and with room for two a batch
        // could never hold more than two pointer moves; with room for 32 a
        // burst leaves in one write, still ahead of every other lane.
        lane("Interactive", "Network", 32, false, false),
        lane("Audio", "NetworkAudio", 2, false, false),
        // Bluetooth does not carry pixels.
        lane("Video", "NetworkVideo", 2, false, true),
        lane("Bulk", "NetworkBulk", 2, true, false),
    ];
    let classes = Priority::ALL
        .into_iter()
        .map(|priority| Class {
            name: name(priority),
            lane: Lane::for_priority(priority).into(),
            budget: budget(priority),
            urgent: matches!(priority, Priority::Control | Priority::Input),
        })
        .collect();
    Arc::new(
        Classes::new(lanes, classes)
            .expect("Cursio's declaration is valid")
            .with_standby_name("Bluetooth")
            .with_window_report(crate::priority::log_window),
    )
}

fn name(priority: Priority) -> &'static str {
    match priority {
        Priority::Control => "control",
        Priority::Input => "input",
        Priority::Audio => "audio",
        Priority::Video => "video",
        Priority::Metadata => "metadata",
        Priority::Bulk => "bulk",
    }
}

fn budget(priority: Priority) -> usize {
    match priority {
        Priority::Control | Priority::Input => 256 * 1024,
        Priority::Audio => 512 * 1024,
        Priority::Video => 4 * 1024 * 1024,
        Priority::Metadata => 1024 * 1024,
        Priority::Bulk => 8 * 1024 * 1024,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_order_is_control_first_bulk_last() {
        let classes = classes();
        for (index, priority) in Priority::ALL.into_iter().enumerate() {
            assert_eq!(priority.class(), index);
            assert_eq!(classes.class(index).name, name(priority));
        }
        assert_eq!(Priority::Control.class(), 0);
        assert!(Priority::Input.class() < Priority::Audio.class());
        assert!(Priority::Audio.class() < Priority::Video.class());
        assert_eq!(Priority::Bulk.class(), classes.classes().len() - 1);
    }

    #[test]
    fn each_class_rides_its_lane_with_its_budget() {
        let classes = classes();
        let budgets: Vec<usize> = classes.classes().iter().map(|c| c.budget).collect();
        assert_eq!(
            budgets,
            [
                256 * 1024,
                256 * 1024,
                512 * 1024,
                4 * 1024 * 1024,
                1024 * 1024,
                8 * 1024 * 1024
            ]
        );
        for priority in Priority::ALL {
            assert_eq!(
                classes.lane_of(priority.class()),
                LaneId::from(Lane::for_priority(priority))
            );
        }
        assert_eq!(classes.lanes().len(), Lane::COUNT);
    }
}
