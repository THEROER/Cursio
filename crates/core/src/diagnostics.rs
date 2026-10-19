//! What is wrong with this machine right now, in words a person can act on.
//!
//! The log already says most of it, once, among thousands of lines: that the
//! USB export failed, that the peer's disk is being walked, that swap is full.
//! A user who looks at the log an hour later finds `no such device` and has to
//! reconstruct from it that `adb` was holding their phone. This is the other
//! half: a small set of *current* problems, each with a stable id, a sentence,
//! and where there is one, the command that fixes it.
//!
//! ## A set, not a stream
//!
//! A problem is a state that holds until it stops holding, so the registry is
//! keyed by id and [`raise`] is idempotent: a producer that notices the same
//! condition every second calls it every second and the problem stays one
//! entry, with the `since` of the first time it was seen — "swap has been full
//! for twenty minutes" is the useful sentence, not "swap became full just now".
//! [`clear`] ends it. A producer never has to remember whether it already
//! raised something, which is what lets it be a one-line call at the site that
//! already detects the condition.
//!
//! ## Machine-wide, and deliberately not per session
//!
//! One registry per process, behind a plain `Mutex`. Problems come from module
//! threads, FUSE callbacks, the pressure monitor and the mesh supervisor alike,
//! and what the user asks is "what is wrong with *this machine*" — a status
//! panel reads [`snapshot`] and needs no route to any of those places. The lock
//! is held only to copy a few small structs, so no producer can stall another.
//!
//! ## Logged once
//!
//! Every raise that changes what the registry says also goes to `tracing` at
//! the level its [`Severity`] names, so the log and the panel never disagree.
//! A raise that repeats an unchanged problem logs nothing: the producers that
//! poll would otherwise print the same line every second for as long as the
//! condition lasted.

use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::ConfigEdit;

/// How much a problem matters. Ordered so that `Error` sorts highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    /// Worth knowing, nothing is broken: a degraded but working path.
    Info,
    /// Something is working worse than it should, or soon will not work.
    Warning,
    /// Something the user asked for is not happening.
    Error,
}

/// One current problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// Stable identity of the condition, e.g. `usb.held.1-1`. Raising the same
    /// id again updates this entry rather than adding one.
    pub id: String,
    pub severity: Severity,
    /// One sentence naming the concrete cause.
    pub summary: String,
    /// What to run or change, when there is something.
    pub hint: Option<String>,
    /// Changes the person can make from where the problem is shown, each a
    /// button. A problem that is a decision the node made on the person's
    /// behalf offers both ways out, so answering it also ends it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ProblemAction>,
    /// When the condition was first seen. Kept across repeated raises.
    pub since: SystemTime,
}

/// One button on a problem: what it says, and the config change it makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemAction {
    pub label: String,
    pub edit: ConfigEdit,
}

static PROBLEMS: Mutex<Vec<Problem>> = Mutex::new(Vec::new());

/// The registry, even if a producer panicked while holding it: a problem list
/// is plain data with no invariant a half-finished update could break.
fn problems() -> std::sync::MutexGuard<'static, Vec<Problem>> {
    PROBLEMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record a problem, or refresh one already recorded under the same id.
///
/// A repeated raise keeps the original `since` and replaces the rest, so a
/// producer can sharpen its sentence (a PID it has just found) without the
/// problem looking new.
pub fn raise(problem: Problem) {
    let changed = {
        let mut list = problems();
        match list.iter_mut().find(|p| p.id == problem.id) {
            Some(existing) => {
                let changed = existing.severity != problem.severity
                    || existing.summary != problem.summary
                    || existing.hint != problem.hint
                    || existing.actions != problem.actions;
                existing.severity = problem.severity;
                existing.summary = problem.summary.clone();
                existing.hint = problem.hint.clone();
                existing.actions = problem.actions.clone();
                changed
            }
            None => {
                list.push(problem.clone());
                true
            }
        }
    };
    if changed {
        log(&problem);
    }
}

/// End a problem. Clearing one that is not recorded is nothing.
pub fn clear(id: &str) {
    let removed = {
        let mut list = problems();
        let before = list.len();
        list.retain(|p| p.id != id);
        list.len() != before
    };
    if removed {
        tracing::info!(problem = id, "resolved");
    }
}

/// Every current problem, the most severe first and the oldest first within a
/// severity.
pub fn snapshot() -> Vec<Problem> {
    let mut list = problems().clone();
    list.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.since.cmp(&b.since)));
    list
}

fn log(problem: &Problem) {
    let hint = problem.hint.as_deref().unwrap_or("");
    match problem.severity {
        Severity::Info => {
            tracing::info!(problem = %problem.id, hint, "{}", problem.summary)
        }
        Severity::Warning => {
            tracing::warn!(problem = %problem.id, hint, "{}", problem.summary)
        }
        Severity::Error => {
            tracing::error!(problem = %problem.id, hint, "{}", problem.summary)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BluetoothMesh;
    use std::time::Duration;

    /// The registry is process-wide and tests run in parallel, so each test
    /// names its own ids and reads back only those.
    fn mine(prefix: &str) -> Vec<Problem> {
        snapshot()
            .into_iter()
            .filter(|p| p.id.starts_with(prefix))
            .collect()
    }

    fn problem(id: &str, severity: Severity, since: SystemTime) -> Problem {
        Problem {
            id: id.to_string(),
            severity,
            summary: format!("{id} is wrong"),
            hint: None,
            actions: Vec::new(),
            since,
        }
    }

    #[test]
    fn raising_the_same_id_twice_keeps_one_entry_and_its_first_since() {
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        raise(problem("t-idem.a", Severity::Warning, first));
        let mut again = problem("t-idem.a", Severity::Error, first + Duration::from_secs(60));
        again.summary = "sharper sentence".into();
        raise(again);

        let got = mine("t-idem.");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].since, first, "a repeat must not look new");
        assert_eq!(got[0].severity, Severity::Error);
        assert_eq!(got[0].summary, "sharper sentence");
    }

    #[test]
    fn clearing_removes_only_the_named_problem() {
        let now = SystemTime::now();
        raise(problem("t-clear.a", Severity::Info, now));
        raise(problem("t-clear.b", Severity::Info, now));
        clear("t-clear.a");
        clear("t-clear.never-raised");

        let ids: Vec<String> = mine("t-clear.").into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["t-clear.b".to_string()]);
    }

    #[test]
    fn snapshot_puts_the_most_severe_first_then_the_oldest() {
        let t = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        raise(problem("t-order.info-old", Severity::Info, t(1)));
        raise(problem("t-order.error-new", Severity::Error, t(30)));
        raise(problem("t-order.warn", Severity::Warning, t(10)));
        raise(problem("t-order.error-old", Severity::Error, t(20)));

        let ids: Vec<String> = mine("t-order.").into_iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            vec![
                "t-order.error-old",
                "t-order.error-new",
                "t-order.warn",
                "t-order.info-old"
            ]
        );
    }

    #[test]
    fn severity_orders_error_above_warning_above_info() {
        assert!(Severity::Error > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
    }

    /// The panel another process draws reads this over serde; a problem must
    /// arrive exactly as it left.
    #[test]
    fn a_problem_round_trips_through_json() {
        let p = Problem {
            id: "usb.held.1-1".into(),
            severity: Severity::Error,
            summary: "POCO X7 Pro is held by adb.exe (PID 42)".into(),
            hint: Some("run `adb kill-server`".into()),
            actions: vec![ProblemAction {
                label: "Keep it off".into(),
                edit: ConfigEdit::BluetoothMesh(BluetoothMesh::Off),
            }],
            since: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        };
        let json = serde_json::to_string(&p).expect("serialises");
        let back: Problem = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, p);
    }
}
