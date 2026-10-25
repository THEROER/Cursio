//! Independent delivery lanes within one authenticated session.
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

    pub(crate) fn decode(value: u8) -> Option<Self> {
        Self::ALL.get(value as usize).copied()
    }
}
