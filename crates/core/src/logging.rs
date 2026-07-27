//! Logging setup — re-exported from the standalone [`cursio_log`] crate.
//!
//! The implementation lives in `cursio-log` so lighter binaries (clipd) can use
//! it without pulling all of `cursio-core`. This module keeps the historical
//! `cursio_core::logging::init(...)` path working.
pub use cursio_log::{init, log_dir, LogGuard};
