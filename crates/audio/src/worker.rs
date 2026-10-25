//! The thread a desktop backend runs one stream or monitor on.
//!
//! PipeWire and WASAPI both run thread-bound loops, so every capture,
//! playback, intent sink and device monitor owns a thread: started with a
//! stop signal, made to report that its loop actually came up (so a stream
//! that cannot open fails its constructor rather than leaving a silently dead
//! thread), and signalled when its owner is dropped. Each backend used to
//! write that out for every kind of stream; the one thing that differed
//! between them is what dropping does, which [`OnDrop`] names.

use std::sync::mpsc;

use crate::AudioError;

/// What dropping a [`Worker`] waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnDrop {
    /// Signal the thread and return at once.
    ///
    /// For every stream: they are dropped from the session loop — a tap when
    /// its app's node goes away, a playback when the peer closes it — and
    /// tearing a PipeWire stream or a WASAPI client down takes hundreds of
    /// milliseconds ("module pump blocked the session loop module=audio
    /// took_ms=380"), felt as a frozen cursor since input rides the same loop.
    /// Nothing needs the thread gone before `drop` returns: it owns its stream
    /// outright and exits on the signal by itself.
    Detach,
    /// Signal the thread and wait for it to end: a device monitor, dropped once
    /// with its node, whose commands must not outlive it.
    Join,
}

/// One backend thread, stopped when this is dropped.
pub(crate) struct Worker {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    on_drop: OnDrop,
}

impl Worker {
    /// Run `run` on a thread called `name` and wait for it to say its loop is
    /// up. `run` is handed the stop signal and the readiness sender; `what`
    /// names the thread in the errors ("capture thread").
    pub(crate) fn start(
        name: String,
        what: &str,
        on_drop: OnDrop,
        run: impl FnOnce(mpsc::Receiver<()>, mpsc::Sender<Result<(), String>>) + Send + 'static,
    ) -> Result<Self, AudioError> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let worker = Self::spawn(name, what, on_drop, move |stop| run(stop, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(worker),
            Ok(Err(e)) => Err(AudioError::Backend(e)),
            Err(_) => Err(AudioError::Backend(format!(
                "{what} exited before signalling readiness"
            ))),
        }
    }

    /// Run `run` on a thread called `name` with no readiness to wait for: a
    /// loop that cannot fail to come up, whose per-item errors it logs itself.
    pub(crate) fn spawn(
        name: String,
        what: &str,
        on_drop: OnDrop,
        run: impl FnOnce(mpsc::Receiver<()>) + Send + 'static,
    ) -> Result<Self, AudioError> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || run(stop_rx))
            .map_err(|e| AudioError::Backend(format!("spawn {what}: {e}")))?;
        Ok(Self {
            stop: Some(stop_tx),
            thread: Some(thread),
            on_drop,
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let thread = self.thread.take();
        if self.on_drop == OnDrop::Join {
            if let Some(thread) = thread {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_loop_that_cannot_open_fails_its_constructor_with_its_reason() {
        let result = Worker::start("t".into(), "test thread", OnDrop::Join, |_, ready| {
            let _ = ready.send(Err("no device".into()));
        });
        assert!(matches!(result, Err(AudioError::Backend(e)) if e == "no device"));
    }

    #[test]
    fn a_thread_that_dies_before_saying_so_is_named_in_the_error() {
        let result = Worker::start("t".into(), "test thread", OnDrop::Join, |_, _| {});
        assert!(
            matches!(result, Err(AudioError::Backend(e)) if e == "test thread exited before signalling readiness")
        );
    }

    #[test]
    fn dropping_a_joined_worker_waits_for_its_loop_to_see_the_stop() {
        let stopped = Arc::new(AtomicBool::new(false));
        let seen = stopped.clone();
        let worker = Worker::start(
            "t".into(),
            "test thread",
            OnDrop::Join,
            move |stop, ready| {
                let _ = ready.send(Ok(()));
                let _ = stop.recv();
                seen.store(true, Ordering::SeqCst);
            },
        )
        .unwrap();
        drop(worker);
        assert!(stopped.load(Ordering::SeqCst));
    }
}
