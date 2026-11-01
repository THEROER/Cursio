//! Log lines an application hands its node, for the node to keep.
//!
//! An application whose log the node cannot open — an Android app writes in
//! its own private directory, which no other app may read — or that keeps no
//! log file at all, still gets `cursio logs <machine> --app <id>`: its lines
//! go over the connection it already has, and the node writes them to a file
//! of its own, read like any other.
//!
//! **Never in the application's way.** [`Cursio::log`](crate::Cursio::log)
//! and the `tracing` layer only push onto a bounded queue and return; the
//! SDK's thread sends what is queued in batches. When the application logs
//! faster than lines can leave, the oldest are dropped and counted, and the
//! node writes how many went missing where they were. The queue outlives a
//! connection, so the lines of a start before the node answered, and of the
//! moments it was away, arrive once it is back.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cursio_app_protocol::{clip_log_line, MAX_LOG_BATCH};
use tokio::sync::Notify;

/// Most lines waiting for the node. Older ones are dropped beyond it.
pub(crate) const QUEUE_LINES: usize = 1000;

/// Lines that make a batch worth sending before the next tick.
pub(crate) const FLUSH_LINES: usize = 64;

/// Most lines in one batch.
pub(crate) const BATCH_LINES: usize = 256;

/// How often waiting lines are sent when fewer than [`FLUSH_LINES`] wait.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_millis(250);

/// How serious a line is, as `tracing` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }
}

/// Lines waiting for the node, bounded, with what fell off counted.
#[derive(Debug, Default)]
pub(crate) struct LogQueue {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

impl LogQueue {
    pub(crate) fn len(&self) -> usize {
        self.lines.len()
    }

    /// Keep `line`, dropping the oldest when full.
    pub(crate) fn push(&mut self, line: &str) {
        let line = clip_log_line(line).to_owned();
        if self.lines.len() == QUEUE_LINES {
            self.drop_oldest();
        }
        self.bytes += line.len();
        self.lines.push_back(line);
    }

    fn drop_oldest(&mut self) {
        if let Some(old) = self.lines.pop_front() {
            self.bytes -= old.len();
            self.dropped += 1;
        }
    }

    /// The oldest lines, up to a batch, and how many were dropped before
    /// them; the count starts again.
    pub(crate) fn take_batch(&mut self) -> Option<(Vec<String>, u64)> {
        if self.lines.is_empty() && self.dropped == 0 {
            return None;
        }
        let mut batch = Vec::new();
        let mut size = 0;
        while let Some(line) = self.lines.front() {
            if batch.len() == BATCH_LINES
                || (size + line.len() > MAX_LOG_BATCH && !batch.is_empty())
            {
                break;
            }
            size += line.len();
            let line = self.lines.pop_front().expect("a front line exists");
            self.bytes -= line.len();
            batch.push(line);
        }
        Some((batch, std::mem::take(&mut self.dropped)))
    }

    /// Put back a batch that could not be sent, ahead of what came since;
    /// what no longer fits counts as dropped.
    pub(crate) fn put_back(&mut self, batch: Vec<String>, dropped: u64) {
        self.dropped += dropped;
        for line in batch.into_iter().rev() {
            if self.lines.len() == QUEUE_LINES {
                self.dropped += 1;
                continue;
            }
            self.bytes += line.len();
            self.lines.push_front(line);
        }
    }
}

/// The queue and what wakes the SDK's thread when a batch is ready, shared
/// between the application's threads and that one.
#[derive(Debug, Default)]
pub(crate) struct LogShared {
    pub(crate) queue: Mutex<LogQueue>,
    pub(crate) ready: Notify,
}

impl LogShared {
    pub(crate) fn push(&self, line: &str) {
        let full = {
            let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
            queue.push(line);
            queue.len() >= FLUSH_LINES
        };
        if full {
            self.ready.notify_one();
        }
    }

    pub(crate) fn take_batch(&self) -> Option<(Vec<String>, u64)> {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take_batch()
    }

    pub(crate) fn put_back(&self, batch: Vec<String>, dropped: u64) {
        self.queue
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .put_back(batch, dropped);
    }
}

/// One line as `tracing`'s default formatter writes it, so the node's log
/// reader orders and merges it with every other log by its timestamp:
/// `2026-11-01T18:51:24.930486Z  INFO target: message`.
pub(crate) fn format_line(at: SystemTime, level: LogLevel, target: &str, message: &str) -> String {
    format!(
        "{} {:>5} {target}: {message}",
        timestamp(at),
        level.as_str()
    )
}

/// `at` in UTC as RFC 3339 with microseconds, as `tracing` writes it.
pub(crate) fn timestamp(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_micros()
    )
}

#[cfg(feature = "tracing")]
pub use layer::CursioLayer;

#[cfg(feature = "tracing")]
mod layer {
    use std::fmt::Write;
    use std::sync::Arc;

    use tracing::field::{Field, Visit};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};

    use super::{format_line, LogLevel, LogShared};

    /// A `tracing` layer that hands every event to the node as a log line;
    /// from [`crate::tracing_layer`].
    #[derive(Debug, Clone)]
    pub struct CursioLayer {
        pub(crate) shared: Arc<LogShared>,
    }

    /// The event's message, then its other fields as `name=value`.
    #[derive(Default)]
    struct Fields {
        message: String,
        rest: String,
    }

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                let _ = write!(self.message, "{value:?}");
            } else {
                let _ = write!(self.rest, " {}={value:?}", field.name());
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.message.push_str(value);
            } else {
                let _ = write!(self.rest, " {}={value}", field.name());
            }
        }
    }

    fn level(level: &Level) -> LogLevel {
        match *level {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            Level::DEBUG => LogLevel::Debug,
            Level::TRACE => LogLevel::Trace,
        }
    }

    impl<S: Subscriber> Layer<S> for CursioLayer {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let meta = event.metadata();
            let mut fields = Fields::default();
            event.record(&mut fields);
            let message = format!("{}{}", fields.message, fields.rest);
            self.shared.push(&format_line(
                std::time::SystemTime::now(),
                level(meta.level()),
                meta.target(),
                message.trim_start(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_reads_as_tracing_writes_it() {
        let at = UNIX_EPOCH + Duration::from_micros(1_793_563_884_930_486);
        assert_eq!(
            format_line(at, LogLevel::Info, "leavepulse::ui", "opened"),
            "2026-11-01T20:11:24.930486Z  INFO leavepulse::ui: opened"
        );
        assert_eq!(
            format_line(at, LogLevel::Error, "x", "y"),
            "2026-11-01T20:11:24.930486Z ERROR x: y"
        );
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_counts_them() {
        let mut queue = LogQueue::default();
        for n in 0..QUEUE_LINES + 5 {
            queue.push(&format!("line {n}"));
        }
        assert_eq!(queue.len(), QUEUE_LINES);
        let (batch, dropped) = queue.take_batch().unwrap();
        assert_eq!(dropped, 5);
        assert_eq!(batch[0], "line 5", "the oldest five went");
        assert_eq!(batch.len(), BATCH_LINES);
        let (_, dropped) = queue.take_batch().unwrap();
        assert_eq!(dropped, 0, "counted once");
    }

    #[test]
    fn a_batch_stops_at_its_byte_limit_and_an_empty_queue_has_none() {
        let mut queue = LogQueue::default();
        let big = "x".repeat(cursio_app_protocol::MAX_LOG_LINE);
        for _ in 0..(MAX_LOG_BATCH / big.len()) + 3 {
            queue.push(&big);
        }
        let (batch, _) = queue.take_batch().unwrap();
        assert!(batch.iter().map(String::len).sum::<usize>() <= MAX_LOG_BATCH);
        while queue.take_batch().is_some() {}
        assert!(queue.take_batch().is_none());
    }

    #[test]
    fn a_batch_that_could_not_leave_goes_back_ahead_of_newer_lines() {
        let mut queue = LogQueue::default();
        queue.push("a");
        queue.push("b");
        let (batch, dropped) = queue.take_batch().unwrap();
        queue.push("c");
        queue.put_back(batch, dropped);
        let (again, _) = queue.take_batch().unwrap();
        assert_eq!(again, ["a", "b", "c"]);
    }

    #[cfg(feature = "tracing")]
    #[test]
    fn a_tracing_event_becomes_one_line_with_its_fields() {
        use tracing_subscriber::layer::SubscriberExt;
        let shared = std::sync::Arc::new(LogShared::default());
        let layer = CursioLayer {
            shared: shared.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "launcher::runner", pid = 42, "game exited");
        });
        let (batch, _) = shared.take_batch().unwrap();
        assert_eq!(batch.len(), 1);
        let line = &batch[0];
        assert!(
            line.ends_with("  WARN launcher::runner: game exited pid=42"),
            "{line}"
        );
        assert_eq!(&line[4..5], "-");
    }
}
