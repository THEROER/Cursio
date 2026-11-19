//! The session bus, reached from one thread of its own.
//!
//! zbus's blocking calls run a future to completion on the calling thread. On
//! a thread inside the node's Tokio runtime — a module's host thread runs
//! inside `Handle::block_on`, startup runs on the runtime itself — that
//! deadlocks: the node built with notifications stopped for good on its first
//! `Connection::session()`, and the update that carried it rolled back. So
//! every call goes to this thread, which belongs to no runtime, and the caller
//! waits on a plain channel for the answer.

use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use zbus::blocking::Connection;

type Job = Box<dyn FnOnce(&Connection) + Send>;

/// The worker's inbox, or `None` when there is no session bus.
fn worker() -> Option<&'static Mutex<mpsc::Sender<Job>>> {
    static WORKER: OnceLock<Option<Mutex<mpsc::Sender<Job>>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
            let (ready_tx, ready_rx) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("session-bus".into())
                .spawn(move || {
                    let bus = match Connection::session() {
                        Ok(bus) => {
                            let _ = ready_tx.send(true);
                            bus
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "no session bus");
                            let _ = ready_tx.send(false);
                            return;
                        }
                    };
                    for job in jobs_rx {
                        job(&bus);
                    }
                });
            match spawned {
                Ok(_) if ready_rx.recv().unwrap_or(false) => Some(Mutex::new(jobs_tx)),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot start the session-bus thread");
                    None
                }
            }
        })
        .as_ref()
}

/// Run `job` against the session bus and answer what it returns, or `None`
/// when there is no bus.
pub fn with_bus<T: Send + 'static>(
    job: impl FnOnce(&Connection) -> T + Send + 'static,
) -> Option<T> {
    let inbox = worker()?;
    let (tx, rx) = mpsc::channel();
    let sent = inbox
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .send(Box::new(move |bus| {
            let _ = tx.send(job(bus));
        }));
    sent.ok()?;
    rx.recv().ok()
}

/// The connection itself, for a thread of the caller's own that only waits
/// on signals. Cloned on the worker, so no blocking call happens here.
pub fn connection() -> Option<Connection> {
    with_bus(|bus| bus.clone())
}
