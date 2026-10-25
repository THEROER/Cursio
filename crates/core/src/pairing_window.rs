//! A time-boxed willingness to meet a machine this node does not trust yet.
//!
//! A mesh node refuses an inbound stranger outright: the trust store is the
//! only way in. Pairing opens a window in which a first meeting may happen,
//! at both ends, keyed by a secret both ends hold — and the deadline closes it
//! again so a node left running overnight is not standing open to the LAN.
//!
//! ## One code instead of one password typed twice
//!
//! `mesh --pair` asks for the same password on both machines. The app flow
//! is one machine *showing* a short code and the other *typing* it, which is
//! the same secret arrived at differently: the code is the SPAKE2 password,
//! and SPAKE2 plus key confirmation run exactly as they always did. Nothing
//! about the protocol is weakened by the secret being short, because SPAKE2
//! gives an eavesdropper nothing to test guesses against offline — every
//! guess costs a live handshake with this node. So the budget that matters is
//! the number of live attempts, and it is small: after
//! [`MAX_FAILED_ATTEMPTS`] failed confirmations the window shuts, and a code
//! window lasts [`CODE_LIFETIME`] and is spent by the first successful
//! pairing. Eight characters from a 31-symbol alphabet is ~2^39 codes against
//! three guesses.
//!
//! The alphabet leaves out `0 O 1 I L` so a code read off one screen and
//! typed on another cannot be misread; input is uppercased and stripped of
//! separators so `abcd efgh` and `ABCD-EFGH` are the same code.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rand::Rng;
use zeroize::Zeroizing;

/// Symbols a code is drawn from: no two of them are mistaken for each other
/// on a screen or when read aloud.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// Characters in a code, not counting the separator.
pub const CODE_LEN: usize = 8;

/// How long a shown code stays usable. Long enough to walk to the other
/// machine and type it; short enough that a code photographed off a screen
/// is useless by the time anyone could try it.
pub const CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// How long `mesh --pair` stays open, as it always has.
pub const PASSWORD_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Failed key confirmations a window tolerates before it shuts. A typo on
/// the other machine costs one; a guesser on the LAN gets no more than this.
pub const MAX_FAILED_ATTEMPTS: u32 = 3;

/// A code in canonical form: [`CODE_LEN`] symbols of [`ALPHABET`].
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(Zeroizing<String>);

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodeError {
    #[error("a pairing code has {CODE_LEN} characters, got {0}")]
    Length(usize),
    #[error("`{0}` is not in a pairing code; codes use 2-9 and A-Z without I, L and O")]
    Symbol(char),
}

impl PairingCode {
    pub fn generate() -> Self {
        let mut rng = rand::rngs::OsRng;
        let code: String = (0..CODE_LEN)
            .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
            .collect();
        Self(Zeroizing::new(code))
    }

    /// Read a code as a person typed it.
    pub fn parse(input: &str) -> Result<Self, CodeError> {
        let cleaned: String = input
            .chars()
            .filter(|c| !matches!(c, '-' | ' ' | '\t'))
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if let Some(bad) = cleaned
            .chars()
            .find(|c| !ALPHABET.contains(&(*c as u8)) || !c.is_ascii())
        {
            return Err(CodeError::Symbol(bad));
        }
        if cleaned.len() != CODE_LEN {
            return Err(CodeError::Length(cleaned.len()));
        }
        Ok(Self(Zeroizing::new(cleaned)))
    }

    /// `ABCD-EFGH`: two halves are easier to hold in the head than eight.
    pub fn display(&self) -> String {
        let (a, b) = self.0.split_at(CODE_LEN / 2);
        format!("{a}-{b}")
    }

    /// The SPAKE2 password this code stands for.
    fn secret(&self) -> Zeroizing<String> {
        self.0.clone()
    }
}

/// How the window's secret came to be, which decides when it closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingKind {
    /// Shown here by this node. One meeting spends it.
    ShownCode,
    /// Typed here, read off another machine. One meeting spends it.
    EnteredCode,
    /// `mesh --pair`'s password, typed on every machine. Stays open for the
    /// whole window, so several machines can join with it.
    Password,
}

/// What a caller may learn about the window without learning its secret.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairingState {
    pub kind: PairingKind,
    /// The code to show, for a window this node opened with one.
    pub code: Option<String>,
    pub expires_at: SystemTime,
    pub failed_attempts: u32,
}

/// Why the last window closed, for a UI waiting on it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PairingOutcome {
    Paired { peer: String },
    Expired,
    TooManyAttempts,
    Cancelled,
}

struct Open {
    kind: PairingKind,
    secret: Zeroizing<String>,
    shown: Option<String>,
    closes_at: Instant,
    expires_at: SystemTime,
    addr: Option<SocketAddr>,
    failed: u32,
}

#[derive(Default)]
struct Inner {
    open: Option<Open>,
    last_outcome: Option<PairingOutcome>,
}

/// The pairing window, shared by the mesh loop and the control socket.
#[derive(Clone, Default)]
pub struct PairingWindow(Arc<Mutex<Inner>>);

impl PairingWindow {
    pub fn closed() -> Self {
        Self::default()
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn open_with(
        &self,
        kind: PairingKind,
        secret: Zeroizing<String>,
        shown: Option<String>,
        lifetime: Duration,
        addr: Option<SocketAddr>,
    ) -> PairingState {
        let open = Open {
            kind,
            secret,
            shown,
            closes_at: Instant::now() + lifetime,
            expires_at: SystemTime::now() + lifetime,
            addr,
            failed: 0,
        };
        let state = describe(&open);
        let mut inner = self.inner();
        inner.open = Some(open);
        inner.last_outcome = None;
        state
    }

    /// Open for `mesh --pair`'s typed password.
    pub fn open_password(
        &self,
        password: Zeroizing<String>,
        addr: Option<SocketAddr>,
    ) -> PairingState {
        self.open_with(
            PairingKind::Password,
            password,
            None,
            PASSWORD_LIFETIME,
            addr,
        )
    }

    /// Open with a fresh code for this node to show.
    pub fn open_shown_code(&self) -> PairingState {
        let code = PairingCode::generate();
        self.open_with(
            PairingKind::ShownCode,
            code.secret(),
            Some(code.display()),
            CODE_LIFETIME,
            None,
        )
    }

    /// Open with a code read off another machine.
    pub fn open_entered_code(&self, code: &PairingCode, addr: Option<SocketAddr>) -> PairingState {
        self.open_with(
            PairingKind::EnteredCode,
            code.secret(),
            None,
            CODE_LIFETIME,
            addr,
        )
    }

    pub fn cancel(&self) {
        let mut inner = self.inner();
        if inner.open.take().is_some() {
            inner.last_outcome = Some(PairingOutcome::Cancelled);
        }
    }

    /// The open window, closing it first if its time is up.
    fn live(inner: &mut Inner) -> Option<&mut Open> {
        if inner
            .open
            .as_ref()
            .is_some_and(|o| o.closes_at <= Instant::now())
        {
            inner.open = None;
            inner.last_outcome = Some(PairingOutcome::Expired);
            tracing::info!("pairing window closed; unknown machines are refused again");
        }
        inner.open.as_mut()
    }

    pub fn is_open(&self) -> bool {
        Self::live(&mut self.inner()).is_some()
    }

    /// The address the user named, while the window is open.
    pub fn addr(&self) -> Option<SocketAddr> {
        Self::live(&mut self.inner()).and_then(|o| o.addr)
    }

    /// The secret to offer a stranger, or empty once the window shut. An
    /// expired window drops the secret, so it stops existing in the process
    /// at the moment it stops being usable.
    pub fn password(&self) -> Zeroizing<String> {
        Self::live(&mut self.inner())
            .map(|o| o.secret.clone())
            .unwrap_or_else(|| Zeroizing::new(String::new()))
    }

    pub fn state(&self) -> Option<PairingState> {
        Self::live(&mut self.inner()).map(|o| describe(o))
    }

    pub fn last_outcome(&self) -> Option<PairingOutcome> {
        let mut inner = self.inner();
        Self::live(&mut inner);
        inner.last_outcome.clone()
    }

    /// A stranger failed key confirmation, or refused ours.
    pub fn note_failure(&self) {
        let mut inner = self.inner();
        let Some(open) = Self::live(&mut inner) else {
            return;
        };
        open.failed += 1;
        tracing::warn!(
            attempt = open.failed,
            "pairing attempt failed; wrong code or password?"
        );
        if open.failed >= MAX_FAILED_ATTEMPTS {
            inner.open = None;
            inner.last_outcome = Some(PairingOutcome::TooManyAttempts);
            tracing::warn!("too many failed pairing attempts; the window is closed");
        }
    }

    /// A stranger became trusted. A code is spent by it; a password is not.
    pub fn note_paired(&self, peer: &str) {
        let mut inner = self.inner();
        let spent = Self::live(&mut inner).is_some_and(|o| o.kind != PairingKind::Password);
        if spent {
            inner.open = None;
        }
        inner.last_outcome = Some(PairingOutcome::Paired {
            peer: peer.to_owned(),
        });
    }

    /// [`link::secure`](crate::link::secure) with this window told how a first
    /// meeting went. Answers the link and whether the peer was a stranger
    /// paired just now.
    ///
    /// Only a wrong secret counts against the window. A dropped socket
    /// mid-handshake is the network, and closing the window for it would
    /// punish the user for Wi-Fi.
    pub async fn secure<T>(
        &self,
        identified: crate::link::Identified<T>,
        plan: &crate::link::Handshake<'_>,
    ) -> anyhow::Result<(crate::link::Link, bool)>
    where
        T: cursio_transport::Transport + 'static,
    {
        use cursio_transport::pairing::{PairingError, TrustStore};

        let peer_id = identified.peer.peer_id;
        let peer_name = identified.peer.display_name.clone();
        let stranger = !plan.password.is_empty()
            && TrustStore::load().is_ok_and(|store| store.get(&peer_id).is_none());
        match crate::link::secure(identified, plan).await {
            Ok(link) => {
                if stranger {
                    self.note_paired(&peer_name);
                }
                Ok((link, stranger))
            }
            Err(error) => {
                if stranger
                    && matches!(
                        error.downcast_ref::<PairingError>(),
                        Some(PairingError::ConfirmationFailed | PairingError::Rejected(_))
                    )
                {
                    self.note_failure();
                }
                Err(error)
            }
        }
    }
}

fn describe(open: &Open) -> PairingState {
    PairingState {
        kind: open.kind,
        code: open.shown.clone(),
        expires_at: open.expires_at,
        failed_attempts: open.failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_code_parses_back_from_its_display_form() {
        for _ in 0..100 {
            let code = PairingCode::generate();
            let shown = code.display();
            assert_eq!(shown.len(), CODE_LEN + 1);
            assert_eq!(PairingCode::parse(&shown).unwrap(), code);
            assert!(!shown.contains(['0', 'O', '1', 'I', 'L']), "{shown}");
        }
    }

    #[test]
    fn a_typed_code_ignores_case_and_separators() {
        let code = PairingCode::parse("ABCD-EFGH").unwrap();
        assert_eq!(PairingCode::parse("abcd efgh").unwrap(), code);
        assert_eq!(PairingCode::parse(" abcdefgh ").unwrap(), code);
    }

    #[test]
    fn a_malformed_code_names_what_is_wrong() {
        assert_eq!(PairingCode::parse("ABC-DEF"), Err(CodeError::Length(6)));
        assert_eq!(PairingCode::parse("ABCD-EFG0"), Err(CodeError::Symbol('0')));
        assert!(PairingCode::parse("ABCD-EFGЖ").is_err());
    }

    #[test]
    fn both_ends_offer_the_same_secret_for_one_code() {
        let shower = PairingWindow::closed();
        let shown = shower.open_shown_code().code.unwrap();
        let typer = PairingWindow::closed();
        typer.open_entered_code(&PairingCode::parse(&shown).unwrap(), None);
        assert_eq!(*shower.password(), *typer.password());
        assert!(!shower.password().is_empty());
    }

    #[test]
    fn a_code_is_spent_by_one_pairing_and_a_password_is_not() {
        let window = PairingWindow::closed();
        window.open_shown_code();
        window.note_paired("laptop");
        assert!(!window.is_open());
        assert_eq!(
            window.last_outcome(),
            Some(PairingOutcome::Paired {
                peer: "laptop".into()
            })
        );

        window.open_password(Zeroizing::new("hunter2".into()), None);
        window.note_paired("laptop");
        assert!(
            window.is_open(),
            "a password admits every machine that knows it"
        );
    }

    #[test]
    fn repeated_failures_close_the_window_and_drop_the_secret() {
        let window = PairingWindow::closed();
        window.open_shown_code();
        for _ in 0..MAX_FAILED_ATTEMPTS - 1 {
            window.note_failure();
        }
        assert!(window.is_open());
        assert_eq!(
            window.state().unwrap().failed_attempts,
            MAX_FAILED_ATTEMPTS - 1
        );
        window.note_failure();
        assert!(!window.is_open());
        assert!(window.password().is_empty());
        assert_eq!(window.last_outcome(), Some(PairingOutcome::TooManyAttempts));
    }

    #[test]
    fn an_expired_window_reads_as_closed_with_no_secret() {
        let window = PairingWindow::closed();
        window.open_with(
            PairingKind::ShownCode,
            Zeroizing::new("ABCDEFGH".into()),
            None,
            Duration::ZERO,
            None,
        );
        assert!(!window.is_open());
        assert!(window.password().is_empty());
        assert_eq!(window.last_outcome(), Some(PairingOutcome::Expired));
    }

    #[test]
    fn a_fresh_code_window_lasts_the_code_lifetime() {
        let state = PairingWindow::closed().open_shown_code();
        let left = state.expires_at.duration_since(SystemTime::now()).unwrap();
        assert!(left <= CODE_LIFETIME && left > CODE_LIFETIME - Duration::from_secs(5));
        assert_eq!(state.kind, PairingKind::ShownCode);
    }
}
