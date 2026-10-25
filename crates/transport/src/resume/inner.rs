//! The link's shared core: its state behind one lock, and what a path's
//! arrival, failure or acknowledgement does to it.

use crate::crypto::{link_tag, open_frame, seal_frame};
use crate::lane::Lane;
use crate::resume::frame::ack_frame;
use crate::resume::frame::read_delivered;
use crate::resume::frame::read_u64;
use crate::resume::state::PathSlot;
use crate::resume::state::State;
use crate::resume::Path;
use crate::resume::PathKind;
use crate::resume::Paths;
use crate::TransportError;
use cursio_protocol::SessionTag;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;

pub(super) type PathsObserver = Box<dyn Fn(Paths) + Send + Sync>;

pub(super) struct Inner {
    pub(super) state: Mutex<State>,
    /// Woken when the path set changes or the link ends.
    pub(super) changed: Notify,
    /// Every path's reader feeds this; the session's receiver drains it.
    pub(super) inbound: [mpsc::Sender<(u64, Vec<u8>)>; Lane::COUNT],
    pub(super) tag: SessionTag,
    pub(super) observer: Mutex<Option<PathsObserver>>,
    pub(super) congestion: Mutex<crate::congestion::BackgroundRate>,
}

impl Inner {
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn kick(&self, flags: u8) {
        let kick = self.lock().kick.clone();
        if let Some(kick) = kick {
            for lane in Lane::ALL {
                kick.push_lane_control(ack_frame([0; Lane::COUNT], flags), lane);
            }
        }
    }

    /// Report the path set if it changed since last reported.
    fn report(&self) {
        let changed = {
            let mut state = self.lock();
            let now = state.summary();
            (now != state.reported).then(|| {
                state.reported = now;
                now
            })
        };
        if let Some(paths) = changed {
            tracing::info!(
                network = paths.network,
                bluetooth = paths.bluetooth,
                "the session's paths changed"
            );
            if let Some(observer) = self
                .observer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
            {
                observer(paths);
            }
        }
    }

    pub(super) fn path_failed(&self, id: u64, why: &str) {
        let removed = {
            let mut state = self.lock();
            let before = state.paths.len();
            state.paths.retain(|p| p.id != id);
            let removed = state.paths.len() != before;
            if removed
                && state.carrying(Lane::Interactive, Instant::now()).is_none()
                && state.ended.is_none()
                && state.pathless_since.is_none()
            {
                state.pathless_since = Some(Instant::now());
            }
            removed
        };
        if removed {
            tracing::info!(path = id, %why, "a path of the session ended");
            self.changed.notify_waiters();
            // The carrying path may have changed: resend on the new one now,
            // not when the session next has something to say.
            self.kick(0);
            self.report();
        }
    }

    pub(super) fn end(&self, why: String) {
        {
            let mut state = self.lock();
            if state.ended.is_some() {
                return;
            }
            tracing::info!(%why, "the session can no longer be resumed; ending it");
            state.ended = Some(why);
            state.paths.clear();
        }
        self.changed.notify_waiters();
        self.report();
    }

    fn probe(&self, id: u64, tagged: &[u8]) {
        let Some(nonce) = tagged
            .get(1..)
            .filter(|bytes| bytes.len() == 8)
            .and_then(read_u64)
        else {
            return;
        };
        let mut state = self.lock();
        let active = state
            .bulk_active
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1));
        let Some(path) = state.paths.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if tagged[0] == link_tag::PING {
            let mut pong = vec![link_tag::PONG];
            pong.extend_from_slice(&nonce.to_be_bytes());
            let _ = path.writer.try_send(pong);
        } else if let Some((expected, at)) = path.probe {
            if expected == nonce {
                path.probe = None;
                self.congestion
                    .lock()
                    .unwrap()
                    .observe(at.elapsed(), Instant::now(), active);
            }
        }
    }

    pub(super) fn acknowledge(&self, bytes: &[u8]) -> Result<(), TransportError> {
        let delivered = read_delivered(bytes)?;
        {
            let mut state = self.lock();
            for lane in Lane::ALL {
                state.acknowledged(lane, delivered[lane as usize]);
            }
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Start a path's reader and writer and add it to the session.
    pub(super) fn install(self: &Arc<Self>, path: Path) {
        let Path {
            mut tx,
            mut rx,
            noise,
            kind,
        } = path;
        let (writer, mut frames) = mpsc::channel::<Vec<u8>>(2);
        let id = {
            let mut state = self.lock();
            state.next_path_id += 1;
            state.next_path_id
        };

        let weak = Arc::downgrade(self);
        let write_noise = noise.clone();
        let write = tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                let sent = match seal_frame(&write_noise, &frame) {
                    Ok(sealed) => tx.send(&sealed).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = sent {
                    if let Some(inner) = weak.upgrade() {
                        inner.path_failed(id, &format!("could not write: {e}"));
                    }
                    return;
                }
            }
        });

        let weak = Arc::downgrade(self);
        let inbound = self.inbound.clone();
        let read = tokio::spawn(async move {
            loop {
                let result = match rx.recv().await {
                    Ok(msg) => open_frame(&noise, msg),
                    Err(e) => Err(e),
                };
                let Some(inner) = weak.upgrade() else { return };
                match result {
                    Ok(tagged) => {
                        if let Some(slot) = inner.lock().paths.iter_mut().find(|p| p.id == id) {
                            slot.last_heard = Instant::now();
                        }
                        if matches!(
                            tagged.first(),
                            Some(&link_tag::PING) | Some(&link_tag::PONG)
                        ) {
                            inner.probe(id, &tagged);
                            continue;
                        }
                        if tagged.first() == Some(&link_tag::ACK) {
                            if let Err(error) = inner.acknowledge(&tagged[1..]) {
                                inner.path_failed(id, &error.to_string());
                                return;
                            }
                            continue;
                        }
                        let Some(lane) = tagged.get(1).and_then(|v| Lane::decode(*v)) else {
                            inner.path_failed(id, "invalid delivery lane");
                            return;
                        };
                        if kind.lane().is_some_and(|expected| expected != lane) {
                            inner.path_failed(id, "frame arrived on the wrong lane");
                            return;
                        }
                        if inbound[lane as usize].send((id, tagged)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        inner.path_failed(id, &format!("could not read: {e}"));
                        return;
                    }
                }
            }
        });

        let now = Instant::now();
        let replaced: Vec<PathSlot> = {
            let mut state = self.lock();
            let (old, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut state.paths)
                .into_iter()
                .partition(|p| p.kind == kind);
            state.paths = keep;
            state.paths.push(PathSlot {
                id,
                kind,
                writer,
                last_heard: now,
                degraded_until: None,
                attached_at: now,
                probe: None,
                last_probe: now,
                tasks: vec![write.abort_handle(), read.abort_handle()],
            });
            if state.carrying(Lane::Interactive, now).is_some() {
                state.pathless_since = None;
            }
            old
        };
        for old in &replaced {
            tracing::info!(
                path = old.id,
                ?kind,
                "a newer path of the same kind replaced this one"
            );
        }
        drop(replaced);
        if matches!(kind, PathKind::Network | PathKind::Bluetooth) {
            *self.congestion.lock().unwrap() = crate::congestion::BackgroundRate::default();
        }
        tracing::info!(path = id, ?kind, "a path joined the session");
        self.changed.notify_waiters();
        self.kick(0);
        self.report();
    }
}
