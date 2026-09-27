//! Which filesystem calls entered and never came back.
//!
//! A mount that stops answering does not look broken from inside: the session
//! reports nothing outstanding, the log keeps flowing, and the process is
//! healthy. It looks broken only to the application, whose threads sit in the
//! kernel waiting for a reply to a request the filesystem believes it finished.
//! Windows puts no deadline on that wait, so those threads never come back,
//! the process cannot be killed, and the machine has to be restarted.
//!
//! Seen live against Steam: nine threads in `EventPairLow` — waiting on the
//! filesystem driver — while this side reported `in_flight=0` and no errors at
//! all. The two accounts cannot both be right, and neither of them says which
//! call went missing.
//!
//! So each callback is counted on the way in and on the way out. A type whose
//! two counts differ, and stays differing, is a call that entered and never
//! returned — which is exactly the thing that cannot be seen any other way.
//! The guard clears on unwind as well, because a callback that panicked also
//! never answered, and its thread is stuck the same way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Calls entered and calls returned, by callback name.
#[derive(Clone, Default)]
pub struct InFlightLog(Arc<Mutex<HashMap<&'static str, (AtomicU64, AtomicU64)>>>);

impl InFlightLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark one call of `what` as entered; the returned guard marks it done.
    pub fn enter(&self, what: &'static str) -> InFlightGuard {
        {
            let mut map = self.0.lock().expect("in-flight log");
            let entry = map
                .entry(what)
                .or_insert_with(|| (AtomicU64::new(0), AtomicU64::new(0)));
            entry.0.fetch_add(1, Ordering::Relaxed);
        }
        InFlightGuard {
            log: self.clone(),
            what,
        }
    }

    fn leave(&self, what: &'static str) {
        let map = self.0.lock().expect("in-flight log");
        if let Some(entry) = map.get(what) {
            entry.1.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Calls that entered and have not returned, by name. Empty when the mount
    /// is idle and every call has been answered.
    pub fn outstanding(&self) -> Vec<(&'static str, u64)> {
        let map = self.0.lock().expect("in-flight log");
        let mut out: Vec<_> = map
            .iter()
            .filter_map(|(what, (in_, out_))| {
                let n = in_
                    .load(Ordering::Relaxed)
                    .saturating_sub(out_.load(Ordering::Relaxed));
                (n > 0).then_some((*what, n))
            })
            .collect();
        out.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        out
    }
}

/// The log every mount in this process reports to.
///
/// One per process rather than one per mount: a stuck call has to be findable
/// without knowing which drive letter it belonged to, and the answer wanted is
/// "what is this process not answering", not "what is drive V not answering".
pub fn shared() -> &'static InFlightLog {
    static LOG: std::sync::OnceLock<InFlightLog> = std::sync::OnceLock::new();
    LOG.get_or_init(InFlightLog::new)
}

/// Say what has been outstanding since the last look, if anything has.
///
/// Called on a timer rather than at the moment of trouble, because by then the
/// process is answering nothing and there is no path through it that could
/// notice. A call named here twice running is one that entered and did not
/// return — every ordinary call is gone within milliseconds.
pub fn report_stuck() {
    let stuck = shared().outstanding();
    if stuck.is_empty() {
        return;
    }
    tracing::warn!(
        calls = %stuck
            .iter()
            .map(|(what, n)| format!("{what}={n}"))
            .collect::<Vec<_>>()
            .join(" "),
        "filesystem calls have entered and not returned; a caller is waiting on us"
    );
}

/// Marks its call finished when dropped, however it finished.
pub struct InFlightGuard {
    log: InFlightLog,
    what: &'static str,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.log.leave(self.what);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call that returned leaves nothing outstanding.
    #[test]
    fn a_call_that_returns_is_not_outstanding() {
        let log = InFlightLog::new();
        {
            let _guard = log.enter("write");
            assert_eq!(log.outstanding(), vec![("write", 1)], "counted on entry");
        }
        assert!(log.outstanding().is_empty(), "and cleared on return");
    }

    /// A call still running is named, which is the whole point: this is the
    /// only place a request that entered and never came back is visible.
    #[test]
    fn a_call_still_running_is_named() {
        let log = InFlightLog::new();
        let _stuck = log.enter("close");
        let _also = log.enter("close");
        let _other = log.enter("read");
        let out = log.outstanding();
        assert_eq!(out[0], ("close", 2), "the busiest first");
        assert!(out.contains(&("read", 1)));
    }

    /// A callback that panicked never answered either, and its thread is stuck
    /// the same way as one that hung.
    #[test]
    fn a_panicking_call_still_counts_as_returned() {
        let log = InFlightLog::new();
        let for_job = log.clone();
        let panicked = std::panic::catch_unwind(move || {
            let _guard = for_job.enter("read");
            panic!("the peer went away");
        });
        assert!(panicked.is_err(), "the call panicked");
        assert!(
            log.outstanding().is_empty(),
            "unwinding is a return as far as the count is concerned"
        );
    }
}
