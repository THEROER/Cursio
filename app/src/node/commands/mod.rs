//! The window's commands, one file per kind of thing the window asks for.

mod actions;
#[cfg(desktop)]
mod files;
mod gallery;
#[cfg(desktop)]
mod messages;
mod settings;

pub use actions::*;
#[cfg(desktop)]
pub use files::*;
pub use gallery::*;
#[cfg(desktop)]
pub use messages::*;
pub use settings::*;
