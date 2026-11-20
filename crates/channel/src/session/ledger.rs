//! What a session still owes its peer, message by message: kept so that a
//! session that ends can hand the application whatever was never
//! acknowledged, and so a message past its deadline is reported, not lost.
//!
//! The resend buffer cannot answer either question. It holds frames, and a
//! chunked message's early frames leave it as soon as they are acknowledged
//! while its last is still unanswered; it holds nothing yet of a message
//! still in the scheduler's queue or in the drainer's hands. So a message is
//! entered here whole, as its encoded body, when the scheduler admits it, and
//! leaves only when the frame that ends it is acknowledged — or when it is
//! reported expired, or handed back as the session ends.
//!
//! Only what asks for it is entered: every reliable message of a session
//! told to hand them back, and any reliable message with a deadline. Cursio
//! asks for neither, so its sessions keep nothing here and pay one branch
//! per message.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::time::SystemTime;

use tokio::sync::mpsc;

use crate::scheduler::MessageLedger;
use crate::session::events::RawEvent;
use crate::ChannelError;

struct Entry {
    body: Vec<u8>,
    deadline: Option<SystemTime>,
}

#[derive(Default)]
struct Book {
    next: u64,
    /// By id, which is the order the messages were admitted in.
    owed: BTreeMap<u64, Entry>,
    /// Messages given up on while some of their frames may still be queued:
    /// those frames are skipped rather than sent as pieces of nothing.
    abandoned: HashSet<u64>,
    ended: bool,
}

pub(super) struct Ledger {
    hand_back: bool,
    book: Mutex<Book>,
    events: mpsc::UnboundedSender<RawEvent>,
}

impl Ledger {
    pub(super) fn new(hand_back: bool, events: mpsc::UnboundedSender<RawEvent>) -> Self {
        Self {
            hand_back,
            book: Mutex::new(Book::default()),
            events,
        }
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The peer acknowledged the frame that ends message `id`.
    pub(super) fn settled(&self, ids: impl IntoIterator<Item = u64>) {
        let mut book = self.book();
        for id in ids {
            book.owed.remove(&id);
        }
    }

    /// Whether `id` was given up on; `last` says this frame ends it, after
    /// which nothing more of it can come.
    pub(super) fn skip(&self, id: u64, last: bool) -> bool {
        let mut book = self.book();
        let skip = book.abandoned.contains(&id);
        if skip && last {
            book.abandoned.remove(&id);
        }
        skip
    }

    /// The session ended: report what had expired and hand back the rest.
    pub(super) fn end(&self) -> Vec<Vec<u8>> {
        let owed = {
            let mut book = self.book();
            book.ended = true;
            book.abandoned.clear();
            std::mem::take(&mut book.owed)
        };
        let now = SystemTime::now();
        let mut handed = Vec::new();
        for entry in owed.into_values() {
            if entry.deadline.is_some_and(|at| now >= at) {
                let _ = self.events.send(RawEvent::Expired { body: entry.body });
            } else {
                handed.push(entry.body);
            }
        }
        handed
    }
}

impl MessageLedger for Ledger {
    fn register(
        &self,
        body: &[u8],
        deadline: Option<SystemTime>,
    ) -> Result<Option<u64>, ChannelError> {
        if !self.tracks(deadline) {
            return Ok(None);
        }
        let mut book = self.book();
        if book.ended {
            return Err(ChannelError::Closed);
        }
        book.next += 1;
        let id = book.next;
        book.owed.insert(
            id,
            Entry {
                body: body.to_vec(),
                deadline,
            },
        );
        Ok(Some(id))
    }

    fn tracks(&self, deadline: Option<SystemTime>) -> bool {
        self.hand_back || deadline.is_some()
    }

    fn expired(&self, id: u64, more_queued: bool) {
        let entry = {
            let mut book = self.book();
            let entry = book.owed.remove(&id);
            if entry.is_some() && more_queued {
                book.abandoned.insert(id);
            }
            entry
        };
        if let Some(entry) = entry {
            // Nobody reading the reports is the application's choice; the
            // message is not delivered either way.
            let _ = self.events.send(RawEvent::Expired { body: entry.body });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn nothing_is_kept_unless_asked_for() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let ledger = Ledger::new(false, tx);
        assert_eq!(ledger.register(b"x", None).unwrap(), None);
        assert!(ledger
            .register(b"y", Some(SystemTime::now()))
            .unwrap()
            .is_some());
    }

    #[test]
    fn what_is_owed_at_the_end_is_handed_back_in_order_and_the_expired_reported() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ledger = Ledger::new(true, tx);
        let a = ledger.register(b"a", None).unwrap().unwrap();
        ledger.register(b"b", None).unwrap();
        ledger
            .register(b"late", Some(SystemTime::now() - Duration::from_secs(1)))
            .unwrap();
        ledger.register(b"c", None).unwrap();
        ledger.settled([a]);
        assert_eq!(ledger.end(), [b"b".to_vec(), b"c".to_vec()]);
        assert!(matches!(rx.try_recv(), Ok(RawEvent::Expired { body }) if body == b"late"));
        assert!(matches!(
            ledger.register(b"d", None),
            Err(ChannelError::Closed)
        ));
    }

    #[test]
    fn the_rest_of_a_message_given_up_on_is_skipped_through_its_last_frame() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ledger = Ledger::new(true, tx);
        let id = ledger.register(b"big", None).unwrap().unwrap();
        ledger.expired(id, true);
        assert!(matches!(rx.try_recv(), Ok(RawEvent::Expired { .. })));
        assert!(ledger.skip(id, false));
        assert!(ledger.skip(id, true));
        assert!(!ledger.skip(id, true));
        ledger.expired(id, false);
        assert!(rx.try_recv().is_err(), "reported once");
    }
}
