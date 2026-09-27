//! A module that waits must not make anything else wait.
//!
//! Every module used to share the session loop with the shared cursor, so an
//! audio thread join, a disk read under a lock or a slow window query froze the
//! mouse and every other module with it. These run a real encrypted session
//! with a deliberately wedged module beside the ones that must keep moving.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cursio_core::module::SessionModule;
use cursio_core::session::{self, SessionError};
use cursio_protocol::{InputEvent, Message, ModuleId, Priority};
use cursio_transport::pairing::Role;
use cursio_transport::tcp::{TcpListenerHandle, TcpTransport};
use cursio_transport::{EncryptedTransport, Transport, TransportError, TransportTx};

async fn encrypted_pair() -> (
    EncryptedTransport<TcpTransport>,
    EncryptedTransport<TcpTransport>,
) {
    let listener = TcpListenerHandle::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr();
    let key = b"shared-key-for-test".to_vec();
    let server = tokio::spawn(async move {
        let t = listener.accept().await.unwrap();
        EncryptedTransport::establish(t, &key, Role::Responder)
            .await
            .unwrap()
    });
    let client_t = TcpTransport::connect(addr).await.unwrap();
    let client = EncryptedTransport::establish(client_t, b"shared-key-for-test", Role::Initiator)
        .await
        .unwrap();
    (client, server.await.unwrap())
}

/// A module message addressed to `module`, tagged so a receiver can tell it
/// apart.
fn module_msg(module: ModuleId, tag: u32) -> Message {
    Message::module(module, Priority::Audio, &tag).unwrap()
}

/// A module that blocks its thread for `stall` in whichever call is chosen,
/// and records every message it is handed.
struct Wedged {
    id: ModuleId,
    stall_pump: Option<Duration>,
    stall_handle: Option<Duration>,
    /// Sent once from `pump`, so a peer can see this module's traffic.
    announce: Option<u32>,
    seen: Arc<Mutex<Vec<(u32, Instant)>>>,
}

impl Wedged {
    fn new(id: ModuleId) -> Self {
        Self {
            id,
            stall_pump: None,
            stall_handle: None,
            announce: None,
            seen: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl SessionModule for Wedged {
    fn name(&self) -> &'static str {
        "wedged"
    }

    fn id(&self) -> ModuleId {
        self.id
    }

    async fn start(&mut self, _tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        Ok(())
    }

    async fn pump(&mut self, tx: &mut dyn TransportTx) -> Result<(), TransportError> {
        if let Some(tag) = self.announce.take() {
            tx.send(&module_msg(self.id, tag)).await?;
        }
        if let Some(stall) = self.stall_pump {
            std::thread::sleep(stall);
        }
        Ok(())
    }

    fn handle_message(&mut self, msg: &Message) -> bool {
        if let Ok(Some(tag)) = msg.decode_module::<u32>(self.id) {
            self.seen.lock().unwrap().push((tag, Instant::now()));
        }
        if let Some(stall) = self.stall_handle {
            std::thread::sleep(stall);
        }
        true
    }
}

fn holder() -> session::CursorSetup {
    session::CursorSetup {
        start_active: true,
        peer_takes_cursor: true,
    }
}

/// Input keeps being injected while a module sits in a pump that blocks for
/// seconds, and so does another module's traffic: the files module waiting on
/// a disk no longer holds the mouse or the audio.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_blocked_in_pump_delays_neither_input_nor_another_module() {
    let (mut client, server) = encrypted_pair().await;

    let mut files = Wedged::new(ModuleId::FILES);
    files.stall_pump = Some(Duration::from_secs(5));
    let audio = Wedged::new(ModuleId::AUDIO);
    let audio_seen = audio.seen.clone();
    let modules: Vec<Box<dyn SessionModule>> = vec![Box::new(files), Box::new(audio)];

    let injected = Arc::new(AtomicUsize::new(0));
    let injected_in_task = injected.clone();
    let server_task = tokio::spawn(async move {
        let _ = session::run_session(
            Box::new(server),
            holder(),
            None,
            move |_| {
                injected_in_task.fetch_add(1, Ordering::Relaxed);
            },
            None,
            |_| {},
            modules,
            || None,
        )
        .await;
    });

    // Let the files module reach its stall before anything is sent.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent_at = Instant::now();
    for _ in 0..50 {
        client
            .send(&Message::Input(InputEvent::PointerMotion { dx: 1, dy: 0 }))
            .await
            .unwrap();
    }
    client.send(&module_msg(ModuleId::AUDIO, 7)).await.unwrap();

    while injected.load(Ordering::Relaxed) < 50 || audio_seen.lock().unwrap().is_empty() {
        assert!(
            sent_at.elapsed() < Duration::from_secs(1),
            "input or audio waited on the blocked files pump: {} injected, audio {:?}",
            injected.load(Ordering::Relaxed),
            audio_seen.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(client);
    server_task.abort();
}

/// A module that takes seconds over one message holds up only its own
/// messages: input after it is injected at once, and another module's message
/// after it arrives at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_blocked_handling_a_message_delays_only_its_own_messages() {
    let (mut client, server) = encrypted_pair().await;

    let mut files = Wedged::new(ModuleId::FILES);
    files.stall_handle = Some(Duration::from_secs(3));
    let files_seen = files.seen.clone();
    let audio = Wedged::new(ModuleId::AUDIO);
    let audio_seen = audio.seen.clone();
    let modules: Vec<Box<dyn SessionModule>> = vec![Box::new(files), Box::new(audio)];

    let injected = Arc::new(AtomicUsize::new(0));
    let injected_in_task = injected.clone();
    let server_task = tokio::spawn(async move {
        let _ = session::run_session(
            Box::new(server),
            holder(),
            None,
            move |_| {
                injected_in_task.fetch_add(1, Ordering::Relaxed);
            },
            None,
            |_| {},
            modules,
            || None,
        )
        .await;
    });

    let sent_at = Instant::now();
    client.send(&module_msg(ModuleId::FILES, 1)).await.unwrap();
    client.send(&module_msg(ModuleId::FILES, 2)).await.unwrap();
    client
        .send(&Message::Input(InputEvent::PointerMotion { dx: 1, dy: 0 }))
        .await
        .unwrap();
    client.send(&module_msg(ModuleId::AUDIO, 3)).await.unwrap();

    while injected.load(Ordering::Relaxed) < 1 || audio_seen.lock().unwrap().is_empty() {
        assert!(
            sent_at.elapsed() < Duration::from_secs(1),
            "input or audio queued behind the files module's slow message"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        files_seen.lock().unwrap().len(),
        1,
        "the files module is still inside its first message, so its second waits"
    );
    // Routed by id: the audio module was handed its own message and nothing of
    // the files module's.
    assert_eq!(
        audio_seen
            .lock()
            .unwrap()
            .iter()
            .map(|(tag, _)| *tag)
            .collect::<Vec<_>>(),
        vec![3]
    );
    drop(client);
    server_task.abort();
}

/// The other direction: this machine's captured input and a healthy module's
/// traffic both reach the peer while a module is blocked in its pump.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_module_delays_neither_forwarded_input_nor_another_modules_sending() {
    let (mut client, server) = encrypted_pair().await;

    let mut files = Wedged::new(ModuleId::FILES);
    files.stall_pump = Some(Duration::from_secs(5));
    let mut audio = Wedged::new(ModuleId::AUDIO);
    audio.announce = Some(42);
    let modules: Vec<Box<dyn SessionModule>> = vec![Box::new(files), Box::new(audio)];

    let (capture_tx, capture_rx) = tokio::sync::mpsc::unbounded_channel();
    let server_task = tokio::spawn(async move {
        let mut capture_rx = capture_rx;
        let _ = session::run_session(
            Box::new(server),
            // Not holding the cursor: this machine's input streams to the peer.
            session::CursorSetup {
                start_active: false,
                peer_takes_cursor: true,
            },
            Some(&mut capture_rx),
            |_| {},
            None,
            |_| {},
            modules,
            || None,
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent_at = Instant::now();
    capture_tx
        .send(InputEvent::PointerMotion { dx: 3, dy: 0 }.into())
        .unwrap();

    let (mut got_input, mut got_audio) = (false, false);
    while !(got_input && got_audio) {
        let msg = tokio::time::timeout(Duration::from_secs(1), client.recv())
            .await
            .expect("the peer heard nothing while one module was blocked")
            .unwrap();
        match msg {
            Message::Input(_) => got_input = true,
            other if other.module_id() == Some(ModuleId::AUDIO) => got_audio = true,
            _ => {}
        }
    }
    assert!(sent_at.elapsed() < Duration::from_secs(1));
    drop(client);
    server_task.abort();
}

/// A peer that goes silent is still noticed while a module is wedged, and the
/// wedged module does not keep the session from ending.
///
/// Paused time, so the fifteen-second timeout elapses virtually; the wedged
/// thread sleeps in real time and is simply left behind.
#[tokio::test(start_paused = true)]
async fn liveness_and_teardown_survive_a_wedged_module() {
    let (client, server) = encrypted_pair().await;
    // Held open and silent: a machine that lost power, not one that hung up.
    let _silent_peer = client;

    let mut files = Wedged::new(ModuleId::FILES);
    files.stall_pump = Some(Duration::from_secs(60));
    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(25),
        session::run_session(
            Box::new(server),
            holder(),
            None,
            |_| {},
            None,
            |_| {},
            vec![Box::new(files) as Box<dyn SessionModule>],
            || None,
        ),
    )
    .await
    .expect("the session hung behind a wedged module");

    assert!(
        matches!(result, Err(SessionError::PeerTimeout(_))),
        "expected the silent peer to time out, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(22),
        "teardown waited for the wedged module: {:?}",
        started.elapsed()
    );
}

/// Ending a session does not wait on a module that will not come out of its
/// pump: the peer says goodbye and the session returns within the grace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wedged_module_does_not_hold_up_ending_the_session() {
    let (mut client, server) = encrypted_pair().await;

    let mut files = Wedged::new(ModuleId::FILES);
    files.stall_pump = Some(Duration::from_secs(60));
    let session = tokio::spawn(session::run_session(
        Box::new(server),
        holder(),
        None,
        |_| {},
        None,
        |_| {},
        vec![Box::new(files) as Box<dyn SessionModule>],
        || None,
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;
    let bye_at = Instant::now();
    client.send(&Message::Bye).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("teardown waited on the wedged module")
        .unwrap();
    assert!(matches!(result, Err(SessionError::PeerLeft)));
    assert!(
        bye_at.elapsed() < Duration::from_secs(5),
        "teardown took {:?}",
        bye_at.elapsed()
    );
}
