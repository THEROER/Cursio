//! The window's commands, one file per kind of thing the window asks for.

mod actions;
mod files;
mod gallery;
mod messages;
mod settings;

pub use actions::*;
pub use files::*;
pub use gallery::*;
pub use messages::*;
pub use settings::*;
