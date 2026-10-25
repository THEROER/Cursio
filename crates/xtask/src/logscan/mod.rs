//! Reading a Cursio log back as numbers.
//!
//! The daemons already measure what matters — input latency percentiles every
//! 30 s, send-queue delay, audio and file-sharing health, and a warning for
//! every stall — but only as log lines. This module turns the lines written
//! during one `live-test` window into a [`Summary`] per machine, and
//! [`evaluate`] turns two summaries into a PASS/FAIL table.
//!
//! Matching is by message text, not by a structured format, because the text
//! *is* the interface: the daemons write `tracing`'s default formatter and
//! xtask deliberately depends on none of the app crates. Messages are matched
//! by a stable prefix so a reworded tail (the slow-turn warning has changed
//! wording more than once) does not silently turn a failure into "no data".

#[cfg(test)]
mod fixture;
mod judge;
mod scan;

pub use judge::{evaluate, failed, render, Machine, Probes};
pub use scan::{field, growth, pressure_full_total_us, summarize, Latency, Stall, Summary};
