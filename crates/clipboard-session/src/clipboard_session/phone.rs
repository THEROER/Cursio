//! The clipboard on a phone, which no `clipboard-core` backend reaches.
//!
//! Android lets an app *write* the clipboard from the background, but since
//! Android 10 it may *read* it only while one of its own windows has focus,
//! and it tells no app in the background that the clipboard changed. A
//! watcher, which is what the module is built around everywhere else, cannot
//! exist there. So a phone plays the two directions differently:
//!
//! - **Computer → phone is automatic.** An offer is pulled at once, as on any
//!   machine, and the text goes onto Android's clipboard through
//!   [`SystemClipboard`] — the platform's half, which the engine's worker
//!   reaches as an ordinary [`Clipboard`] ([`PhoneSide`]). Text only: an image
//!   on Android's clipboard is a content URI that some provider must keep
//!   serving for as long as the clip lives, which is a file store of its own,
//!   not a write.
//! - **Phone → computer is one tap.** A tile brings a window forward for the
//!   moment it takes to read the clip and hands the text to
//!   [`PhoneClipboard::hand`], addressed to one computer's link; that link
//!   offers it exactly like a local copy, and the computer pulls it as usual.
//!   Nothing reads the phone's clipboard any other way.
//!
//! **The offer a link sends as it comes up is not taken.** That offer is
//! whatever was already on the computer's clipboard, not something the user
//! just copied. Between two desks it is how they agree from the start; but a
//! phone walks in and out of Wi-Fi all day, and putting the computer's old
//! clipboard over whatever the user had copied on the phone at every return
//! is a worse loss than a copy made in the link's first seconds
//! ([`STARTUP_GRACE`]).
//!
//! **Each copy reaches Android once.** With two computers linked, the second
//! learns a copy from the first and offers it on as its own; both links pull
//! it. What was last put is shared by every link (`last_put`), so the second
//! write is recognised and skipped rather than shown to the user twice by
//! Android's own "copied" toast. There is no echo the other way: the phone
//! never watches, so writing the clipboard can never look like a copy here.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use clipboard_core::{Clipboard, ClipboardContent, ClipboardError, ClipboardFormat, Selection};
use cursio_protocol::{ModuleId, PeerId};

use crate::module::ModuleSwitches;

/// How long after a link comes up its offers count as what the computer
/// already held rather than a fresh copy. The first offer goes out on the
/// computer's first pump after the module starts, well inside this.
pub const STARTUP_GRACE: Duration = Duration::from_secs(3);

/// What a phone pulls: the one representation Android's clipboard takes
/// without a provider behind it.
pub(super) const PULLABLE: &[ClipboardFormat] = &[ClipboardFormat::Text];

/// The platform's clipboard on a phone: written, never read from here.
pub trait SystemClipboard: Send + Sync {
    /// Put `text` on the system clipboard, answering why not.
    fn put_text(&self, text: &str) -> Result<(), String>;
}

/// Where a tap's clipboard goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    To {
        peer: PeerId,
        name: String,
    },
    /// More than one computer, none known to have the user: the phone asks.
    Pick(Vec<(PeerId, String)>),
    /// Why it can go nowhere.
    Nowhere(String),
}

/// The phone's clipboard, shared by every link's clipboard module.
#[derive(Clone)]
pub struct PhoneClipboard {
    inner: Arc<Inner>,
}

struct Inner {
    system: Arc<dyn SystemClipboard>,
    switches: ModuleSwitches,
    last_put: Mutex<Option<String>>,
    links: Mutex<HashMap<PeerId, LinkEntry>>,
}

struct LinkEntry {
    token: u64,
    name: String,
    computer: bool,
    tx: mpsc::Sender<ClipboardContent>,
}

impl PhoneClipboard {
    /// `switches` is the node's own: `modules.clipboard` turned off stops
    /// both directions on links already up, not only on the next one.
    pub fn new(system: Arc<dyn SystemClipboard>, switches: ModuleSwitches) -> Self {
        Self {
            inner: Arc::new(Inner {
                system,
                switches,
                last_put: Mutex::new(None),
                links: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.switches.is_on(ModuleId::CLIPBOARD)
    }

    /// Where a tap sends the clipboard: the computer the user was at last
    /// (`present`, from what the computers report about their input), else
    /// the only computer, else the user picks. Another phone is never a
    /// destination — there is nobody at it to paste.
    pub fn destination(&self, present: Option<PeerId>) -> Destination {
        if !self.enabled() {
            return Destination::Nowhere("clipboard sharing is off in Cursio's settings".into());
        }
        let mut computers: Vec<(PeerId, String)> = lock(&self.inner.links)
            .iter()
            .filter(|(_, l)| l.computer)
            .map(|(id, l)| (*id, l.name.clone()))
            .collect();
        computers.sort_by(|a, b| a.1.cmp(&b.1));
        if let Some((peer, name)) = present.and_then(|p| computers.iter().find(|c| c.0 == p)) {
            return Destination::To {
                peer: *peer,
                name: name.clone(),
            };
        }
        match computers.len() {
            0 => Destination::Nowhere("no computer is connected with clipboard sharing on".into()),
            1 => {
                let (peer, name) = computers.remove(0);
                Destination::To { peer, name }
            }
            _ => Destination::Pick(computers),
        }
    }

    /// Offer `text`, read from the phone's clipboard by the user's tap, to
    /// `peer`, by id or by name as [`Destination`] gave it. Answers the
    /// computer's name.
    pub fn hand(&self, peer: &str, text: String) -> Result<String, String> {
        if !self.enabled() {
            return Err("clipboard sharing is off in Cursio's settings".into());
        }
        if text.is_empty() {
            return Err("the clipboard holds no text".into());
        }
        let links = lock(&self.inner.links);
        let link = links
            .iter()
            .find(|(id, l)| id.to_string() == peer || l.name.eq_ignore_ascii_case(peer))
            .map(|(_, l)| l)
            .ok_or("that computer is not connected with clipboard sharing on")?;
        link.tx
            .send(ClipboardContent::text(text))
            .map_err(|_| "that computer's link just closed".to_string())?;
        Ok(link.name.clone())
    }

    /// Register one link's module; what is handed for `peer` arrives on the
    /// returned link. A newer link to the same peer replaces this one, and
    /// this one's end then leaves it alone.
    pub(super) fn attach(&self, peer: PeerId, name: String, computer: bool) -> PhoneLink {
        let (tx, rx) = mpsc::channel();
        let token = rand::random();
        lock(&self.inner.links).insert(
            peer,
            LinkEntry {
                token,
                name,
                computer,
                tx,
            },
        );
        PhoneLink {
            phone: self.clone(),
            peer,
            token,
            handed: rx,
        }
    }

    /// Put a received copy on the system clipboard, unless it is the one
    /// last put there by any link.
    fn put(&self, text: &str) -> Result<bool, ClipboardError> {
        let mut last = lock(&self.inner.last_put);
        if last.as_deref() == Some(text) {
            return Ok(false);
        }
        self.inner
            .system
            .put_text(text)
            .map_err(ClipboardError::Backend)?;
        *last = Some(text.to_string());
        Ok(true)
    }
}

/// One link's registration with [`PhoneClipboard`]; dropping it unregisters.
pub(super) struct PhoneLink {
    phone: PhoneClipboard,
    peer: PeerId,
    token: u64,
    handed: mpsc::Receiver<ClipboardContent>,
}

impl PhoneLink {
    pub(super) fn phone(&self) -> &PhoneClipboard {
        &self.phone
    }

    /// The newest copy handed for this link since the last call, if any.
    pub(super) fn take_handed(&self) -> Option<ClipboardContent> {
        self.handed.try_iter().last()
    }
}

impl Drop for PhoneLink {
    fn drop(&mut self) {
        let mut links = lock(&self.phone.inner.links);
        if links.get(&self.peer).is_some_and(|l| l.token == self.token) {
            links.remove(&self.peer);
        }
    }
}

/// The phone's clipboard as one link's worker sees it: what this link last
/// wrote or was handed reads back, and a write goes to the system.
pub(super) struct PhoneSide {
    pub(super) phone: PhoneClipboard,
    pub(super) current: Arc<Mutex<Option<ClipboardContent>>>,
}

impl Clipboard for PhoneSide {
    fn read(&mut self, _selection: Selection) -> Result<ClipboardContent, ClipboardError> {
        lock(&self.current).clone().ok_or(ClipboardError::Empty)
    }

    fn load(
        &mut self,
        _selection: Selection,
        format: &ClipboardFormat,
    ) -> Result<Vec<u8>, ClipboardError> {
        lock(&self.current)
            .as_ref()
            .and_then(|c| c.bytes(format).map(ToOwned::to_owned))
            .ok_or_else(|| ClipboardError::FormatUnavailable(format.mime().into()))
    }

    fn write(
        &mut self,
        _selection: Selection,
        content: &ClipboardContent,
    ) -> Result<(), ClipboardError> {
        let text = content
            .as_text()
            .ok_or_else(|| ClipboardError::FormatUnavailable("text/plain".into()))?;
        if !self.phone.put(text)? {
            tracing::debug!("copy already on the phone's clipboard; not putting it twice");
        }
        *lock(&self.current) = Some(content.clone());
        Ok(())
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorded(Mutex<Vec<String>>);

    impl SystemClipboard for Recorded {
        fn put_text(&self, text: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn phone() -> (PhoneClipboard, Arc<Recorded>, ModuleSwitches) {
        let system = Arc::new(Recorded::default());
        let switches = ModuleSwitches::default();
        (
            PhoneClipboard::new(system.clone(), switches.clone()),
            system,
            switches,
        )
    }

    fn side(phone: &PhoneClipboard) -> PhoneSide {
        PhoneSide {
            phone: phone.clone(),
            current: Arc::default(),
        }
    }

    fn id(n: u8) -> PeerId {
        PeerId::from_u128(u128::from(n))
    }

    #[test]
    fn a_copy_two_computers_offer_reaches_the_system_clipboard_once() {
        let (phone, system, _) = phone();
        let (mut first, mut second) = (side(&phone), side(&phone));
        let copy = ClipboardContent::text("https://example.org/a,b");
        first.write(Selection::Clipboard, &copy).unwrap();
        second.write(Selection::Clipboard, &copy).unwrap();
        assert_eq!(*system.0.lock().unwrap(), vec!["https://example.org/a,b"]);
        // Each link still reads back what it wrote, for its own echo check.
        assert_eq!(second.read(Selection::Clipboard).unwrap(), copy);
    }

    #[test]
    fn a_new_copy_after_the_same_one_is_put_again() {
        let (phone, system, _) = phone();
        let mut link = side(&phone);
        for text in ["a", "b", "a"] {
            link.write(Selection::Clipboard, &ClipboardContent::text(text))
                .unwrap();
        }
        assert_eq!(*system.0.lock().unwrap(), vec!["a", "b", "a"]);
    }

    #[test]
    fn a_copy_without_text_is_refused_rather_than_put_empty() {
        let (phone, system, _) = phone();
        let png = ClipboardContent::single(ClipboardFormat::ImagePng, vec![1, 2, 3]);
        assert!(side(&phone).write(Selection::Clipboard, &png).is_err());
        assert!(system.0.lock().unwrap().is_empty());
    }

    #[test]
    fn the_tap_goes_to_the_computer_the_user_is_at() {
        let (phone, _, _) = phone();
        let _a = phone.attach(id(1), "Nocturne".into(), true);
        let _b = phone.attach(id(2), "Lightmoon".into(), true);
        assert_eq!(
            phone.destination(Some(id(2))),
            Destination::To {
                peer: id(2),
                name: "Lightmoon".into()
            }
        );
    }

    #[test]
    fn with_nobody_known_present_two_computers_are_a_choice_and_one_is_not() {
        let (phone, _, _) = phone();
        let a = phone.attach(id(1), "Nocturne".into(), true);
        let _b = phone.attach(id(2), "Lightmoon".into(), true);
        let _tablet = phone.attach(id(3), "Tablet".into(), false);
        assert_eq!(
            phone.destination(None),
            Destination::Pick(vec![
                (id(2), "Lightmoon".into()),
                (id(1), "Nocturne".into())
            ])
        );
        drop(a);
        assert_eq!(
            phone.destination(Some(id(1))),
            Destination::To {
                peer: id(2),
                name: "Lightmoon".into()
            }
        );
    }

    #[test]
    fn another_phone_is_never_a_destination() {
        let (phone, _, _) = phone();
        let _tablet = phone.attach(id(3), "Tablet".into(), false);
        assert!(matches!(
            phone.destination(Some(id(3))),
            Destination::Nowhere(_)
        ));
    }

    #[test]
    fn switched_off_the_clipboard_goes_nowhere() {
        let (phone, _, switches) = phone();
        let link = phone.attach(id(1), "Nocturne".into(), true);
        switches.set(ModuleId::CLIPBOARD, false);
        assert!(matches!(phone.destination(None), Destination::Nowhere(_)));
        assert!(phone.hand(&id(1).to_string(), "x".into()).is_err());
        assert!(link.take_handed().is_none());
    }

    #[test]
    fn handed_text_reaches_that_link_only_and_the_newest_wins() {
        let (phone, _, _) = phone();
        let a = phone.attach(id(1), "Nocturne".into(), true);
        let b = phone.attach(id(2), "Lightmoon".into(), true);
        assert_eq!(
            phone.hand(&id(1).to_string(), "old".into()).unwrap(),
            "Nocturne"
        );
        phone.hand(&id(1).to_string(), "new".into()).unwrap();
        assert_eq!(a.take_handed(), Some(ClipboardContent::text("new")));
        assert!(b.take_handed().is_none());
    }

    #[test]
    fn an_older_links_end_leaves_the_newer_link_registered() {
        let (phone, _, _) = phone();
        let old = phone.attach(id(1), "Nocturne".into(), true);
        let new = phone.attach(id(1), "Nocturne".into(), true);
        drop(old);
        phone.hand(&id(1).to_string(), "x".into()).unwrap();
        assert!(new.take_handed().is_some());
    }
}
