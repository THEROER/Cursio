//! What a channel is built from: classes, highest priority first, and the
//! lanes they ride.
//!
//! A lane is one carrier connection with its own numbering, acknowledgements
//! and resend buffer, so a stalled bulk socket cannot hold the interactive
//! one. A class is a queue within a lane, with a byte budget enforced at
//! admission. Each user declares its own list: Cursio's six classes on four
//! lanes are one such list, a console with a bulk lane another.
//!
//! The first lane is the interactive one. It carries the session's own
//! maintenance on a standby path, it is the lane whose round trip paces
//! background traffic, and input stalling on it is what moves a session to
//! another path; the scheduler of a link over one connection drains every
//! class through it.

use crate::{ChannelError, QueueDelay};

/// Index of a class in its [`Classes`], highest priority first.
pub type ClassId = usize;

/// One lane of a session, by its index in [`Classes::lanes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LaneId(u8);

impl LaneId {
    /// The interactive lane; see the module docs.
    pub const INTERACTIVE: Self = Self(0);

    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// One class of traffic.
#[derive(Debug, Clone)]
pub struct Class {
    /// How logs name it.
    pub name: &'static str,
    pub lane: LaneId,
    /// Bytes this class may hold queued. A reliable producer waits for room;
    /// one message larger than the budget is admitted into an empty class.
    pub budget: usize,
    /// Never waits for a bulk message the starvation guard forces through:
    /// session control and input, which cannot starve anything since neither
    /// stays non-empty, and which a 64 MiB release would otherwise stall.
    pub urgent: bool,
}

/// One lane.
#[derive(Debug, Clone)]
pub struct Lane {
    /// How logs name the lane.
    pub name: &'static str,
    /// How logs name a path that carries this lane alone.
    pub path_name: &'static str,
    /// How many frames of this lane may wait for a path's writer. Small for
    /// lanes of large frames, so their order stays with the scheduler rather
    /// than in a queue it cannot see; larger for a lane of tiny frames, so a
    /// burst leaves in one write.
    pub writer_depth: usize,
    /// Paced against the interactive lane's round trip (background traffic).
    pub paced: bool,
    /// A standby path drops this lane's fresh frames rather than carry them:
    /// pixels over a radio that cannot keep up with them.
    pub standby_skips_fresh: bool,
}

/// A finished measurement window of the scheduler: what each class waited,
/// and how many of its messages were dropped unsent for age.
#[derive(Debug, Clone)]
pub struct Window {
    pub delay: Vec<QueueDelay>,
    pub expired: Vec<u32>,
}

/// The classes and lanes of a channel.
#[derive(Debug, Clone)]
pub struct Classes {
    classes: Vec<Class>,
    lanes: Vec<Lane>,
    standby_name: &'static str,
    report: fn(&Classes, &Window),
}

impl Classes {
    /// Declare a channel. Refused when a class names a lane that does not
    /// exist, when a lane has no class (its maintenance would have nowhere to
    /// queue), or past 255 lanes, which the wire numbers in a byte.
    pub fn new(lanes: Vec<Lane>, classes: Vec<Class>) -> Result<Self, ChannelError> {
        if lanes.is_empty() || lanes.len() > usize::from(u8::MAX) {
            return Err(ChannelError::Other(format!(
                "a channel has between 1 and 255 lanes, not {}",
                lanes.len()
            )));
        }
        if let Some(class) = classes.iter().find(|c| c.lane.index() >= lanes.len()) {
            return Err(ChannelError::Other(format!(
                "class {} rides lane {}, which is not declared",
                class.name,
                class.lane.get()
            )));
        }
        if let Some(lane) = (0..lanes.len()).find(|&l| !classes.iter().any(|c| c.lane.index() == l))
        {
            return Err(ChannelError::Other(format!(
                "lane {} has no class",
                lanes[lane].name
            )));
        }
        Ok(Self {
            classes,
            lanes,
            standby_name: "Standby",
            report: log_window,
        })
    }

    /// How logs name a standby path, one able to carry every lane.
    pub fn with_standby_name(mut self, name: &'static str) -> Self {
        self.standby_name = name;
        self
    }

    /// Report each finished delay window with `report` instead of the
    /// generic log line.
    pub fn with_window_report(mut self, report: fn(&Classes, &Window)) -> Self {
        self.report = report;
        self
    }

    pub fn classes(&self) -> &[Class] {
        &self.classes
    }

    pub fn lanes(&self) -> &[Lane] {
        &self.lanes
    }

    pub fn class(&self, id: ClassId) -> &Class {
        &self.classes[id]
    }

    pub fn lane(&self, lane: LaneId) -> &Lane {
        &self.lanes[lane.index()]
    }

    pub fn lane_of(&self, class: ClassId) -> LaneId {
        self.classes[class].lane
    }

    pub fn lane_ids(&self) -> impl Iterator<Item = LaneId> {
        (0..self.lanes.len() as u8).map(LaneId)
    }

    pub fn decode_lane(&self, value: u8) -> Option<LaneId> {
        (usize::from(value) < self.lanes.len()).then_some(LaneId(value))
    }

    pub fn standby_name(&self) -> &'static str {
        self.standby_name
    }

    /// The class a lane's own maintenance queues in: its most urgent.
    pub(crate) fn maintenance_class(&self, lane: LaneId) -> ClassId {
        self.classes
            .iter()
            .position(|c| c.lane == lane)
            .expect("every lane has a class")
    }

    pub(crate) fn report(&self, window: &Window) {
        (self.report)(self, window);
    }
}

/// Report a finished window: what was dropped for age, if anything, and each
/// class that sent something.
fn log_window(classes: &Classes, window: &Window) {
    let expired: u32 = window.expired.iter().sum();
    if expired > 0 {
        tracing::info!(
            expired,
            "dropped messages too old to be worth sending; the link stalled"
        );
    }
    for (class, delay) in classes.classes().iter().zip(&window.delay) {
        if delay.frames > 0 {
            tracing::debug!(
                class = class.name,
                frames = delay.frames,
                p50 = ?delay.p50,
                p99 = ?delay.p99,
                max = ?delay.max,
                "send queue delay"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(name: &'static str) -> Lane {
        Lane {
            name,
            path_name: name,
            writer_depth: 2,
            paced: false,
            standby_skips_fresh: false,
        }
    }

    fn class(name: &'static str, lane: u8) -> Class {
        Class {
            name,
            lane: LaneId::new(lane),
            budget: 1024,
            urgent: false,
        }
    }

    #[test]
    fn a_class_on_an_undeclared_lane_is_refused() {
        assert!(Classes::new(vec![lane("a")], vec![class("x", 1)]).is_err());
    }

    #[test]
    fn a_lane_without_a_class_is_refused() {
        assert!(Classes::new(vec![lane("a"), lane("b")], vec![class("x", 0)]).is_err());
    }

    #[test]
    fn a_lanes_maintenance_queues_in_its_most_urgent_class() {
        let classes = Classes::new(
            vec![lane("a"), lane("b")],
            vec![class("x", 0), class("y", 1), class("z", 1)],
        )
        .unwrap();
        assert_eq!(classes.maintenance_class(LaneId::new(1)), 1);
        assert_eq!(classes.decode_lane(2), None);
    }
}
