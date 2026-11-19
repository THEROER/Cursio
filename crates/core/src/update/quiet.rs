//! Whether now is a moment the node can be restarted without anyone noticing.
//!
//! The activity the sessions and modules report lives in
//! [`cursio_core_base::quiet`], below every module that reports it; what is
//! added here is the one input read from above it: the calls up on the phones
//! this machine sees.

pub use cursio_core_base::quiet::*;

/// Calls up on every phone this machine sees, ringing ones included.
pub fn calls_up() -> usize {
    crate::calls_hub::hub()
        .phones()
        .iter()
        .map(|p| p.calls.len())
        .sum()
}

/// Why now is not a quiet moment, or `None` when it is.
pub fn busy() -> Option<String> {
    busy_with(calls_up())
}
