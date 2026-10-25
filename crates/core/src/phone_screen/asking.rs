//! The computer's side: a phone's screen offered as a remote-screen source, and
//! the request that asks the phone's user to start it.

use crate::phone_screen::feed;
use crate::phone_screen::requested;
use crate::phone_screen::title;
use crate::phone_screen::PhoneScreenSession;
use cursio_protocol::WindowId;
use cursio_window::{CaptureSession, WindowError};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

/// The phone's screen as a source for a computer's capture apps (Discord,
/// OBS), alongside the window a computer can open.
pub struct PhoneScreenSources {
    pub phone: String,
}

/// How long a capture app's pick waits for the phone's user to allow it.
pub(super) const CONSENT_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

impl cursio_window::ScreenSources for PhoneScreenSources {
    fn list(&self) -> Result<Vec<cursio_protocol::ScreenSource>, WindowError> {
        let size = feed().running.map_or((0, 0), |r| (r.width, r.height));
        Ok(vec![cursio_protocol::ScreenSource {
            id: 1,
            kind: cursio_protocol::SourceKind::Monitor,
            title: title(&self.phone),
            width: size.0,
            height: size.1,
            // The phone asks its user before anything streams.
            picked_on_host: true,
        }])
    }

    /// Blocks until the phone's user allows the capture, or gives up.
    fn open(
        &self,
        id: WindowId,
        _source: &cursio_protocol::ScreenSource,
    ) -> Result<Box<dyn CaptureSession + Send>, WindowError> {
        const VIEW: &str = "a capture app";
        if feed().running.is_none() {
            requested(VIEW, true);
            let deadline = std::time::Instant::now() + CONSENT_WAIT;
            while feed().running.is_none() {
                if std::time::Instant::now() >= deadline {
                    requested(VIEW, false);
                    return Err(WindowError::WindowGone(
                        "the phone's user did not allow sharing its screen".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        } else {
            feed().viewers.insert(VIEW.to_string());
        }
        let running = feed().running.ok_or_else(|| {
            WindowError::WindowGone("this phone's screen is not being captured".into())
        })?;
        Ok(Box::new(PhoneScreenSession::open(
            id,
            title(&self.phone),
            running,
            Some(VIEW.to_string()),
        )))
    }
}

/// The computer's side: each link to a phone, by the phone's name, and the
/// channel that link's window engine sends [`WindowMsg::RequestScreen`] from.
///
/// [`WindowMsg::RequestScreen`]: cursio_protocol::WindowMsg::RequestScreen
pub(super) fn viewers() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<bool>>> {
    static VIEWERS: OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<bool>>>> = OnceLock::new();
    VIEWERS.get_or_init(Default::default)
}

/// Register a link to `phone`; its engine sends what the receiver yields.
pub fn register_phone(phone: &str) -> mpsc::UnboundedReceiver<bool> {
    let (tx, rx) = mpsc::unbounded_channel();
    viewers()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(phone.to_string(), tx);
    rx
}

/// Ask `phone` to show its screen here, or to stop. Answers with the phone's
/// name as it is known, or why it cannot be asked.
pub fn ask(phone: &str, show: bool) -> Result<String, String> {
    let mut viewers = viewers().lock().unwrap_or_else(|p| p.into_inner());
    // A link that ended leaves a closed sender behind.
    viewers.retain(|_, tx| !tx.is_closed());
    let found = viewers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(phone))
        .map(|(name, tx)| (name.clone(), tx.clone()));
    match found {
        Some((name, tx)) => {
            let _ = tx.send(show);
            Ok(name)
        }
        None => {
            let mut known: Vec<&str> = viewers.keys().map(String::as_str).collect();
            known.sort_unstable();
            Err(if known.is_empty() {
                format!("{phone} cannot show its screen here: no phone is connected with screen sharing")
            } else {
                format!(
                    "{phone} cannot show its screen here; phones that can: {}",
                    known.join(", ")
                )
            })
        }
    }
}
