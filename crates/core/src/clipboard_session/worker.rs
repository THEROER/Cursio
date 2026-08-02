//! The blocking clipboard, off the session loop.
//!
//! `clipboard-core`'s Linux backend is synchronous and thread-affine: the
//! `arboard` connection must stay on one thread, `read`/`load` block, and
//! `write` detaches its own serving thread. None of that can run on the async
//! session loop. So a single worker thread owns the clipboard and everything
//! else talks to it through channels — the module above stays `Send` and never
//! blocks.
//!
//! The worker does three jobs:
//! - **watch**: forward the platform watcher's change notifications, coalesced
//!   into a single "something changed" flag the module polls;
//! - **list**: on demand, read the current clipboard's formats and sizes
//!   (payload-free) for an offer;
//! - **read**: on demand, load one representation's bytes to serve a peer;
//! - **write**: put received content onto the local clipboard.
//!
//! Requests and replies are plain channels; the module drives them from `pump`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clipboard_core::{
    Clipboard, ClipboardContent, ClipboardError, ClipboardFormat, Selection, WatchHandle,
};
use cursio_protocol::{ClipboardFormatOffer, ClipboardGeneration};

/// A job handed to the worker thread.
enum Job {
    /// Read the current clipboard's formats+sizes (no payloads) and reply on the
    /// oneshot. Used to build an offer.
    List(std::sync::mpsc::Sender<Result<Vec<ClipboardFormatOffer>, ClipboardError>>),
    /// Load one representation's bytes to serve the peer. Tagged with the
    /// generation so the reply carries it back for staleness checks.
    Read {
        generation: ClipboardGeneration,
        format: ClipboardFormat,
    },
    /// Put received content onto the local clipboard.
    Write(ClipboardContent),
    /// Check whether the clipboard still contains a copy we wrote. Used to
    /// distinguish the watch notification caused by that write from a real
    /// local copy, so two peers do not echo the same payload forever.
    Matches(
        ClipboardContent,
        std::sync::mpsc::Sender<Result<bool, ClipboardError>>,
    ),
}

/// The worker's answer to a [`Job::Read`].
pub enum ReadReply {
    Ok {
        generation: ClipboardGeneration,
        format: ClipboardFormat,
        bytes: Vec<u8>,
    },
    Failed {
        generation: ClipboardGeneration,
        format: ClipboardFormat,
        error: ClipboardError,
    },
}

/// Handle to the clipboard worker thread. Dropping it stops the thread.
pub struct ClipboardWorker {
    /// Jobs to the worker.
    jobs: std::sync::mpsc::Sender<Job>,
    /// Read replies back from the worker.
    reads: std::sync::mpsc::Receiver<ReadReply>,
    /// Set by the watch thread when the clipboard changes; cleared when polled.
    changed: Arc<AtomicBool>,
    /// Kept alive so the watch runs; dropped on [`stop`](Self::stop).
    watch: Option<WatchHandle>,
    /// Signals the worker thread to exit.
    stop: Arc<AtomicBool>,
}

impl ClipboardWorker {
    /// Spawn the worker thread. It opens the clipboard backend lazily on first
    /// use so a session on a headless/unsupported host still starts (the module
    /// simply never offers or serves anything).
    pub fn spawn() -> Self {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<Job>();
        let (reads_tx, reads_rx) = std::sync::mpsc::channel::<ReadReply>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        std::thread::Builder::new()
            .name("clipboard-worker".into())
            .spawn(move || run_worker(jobs_rx, reads_tx, thread_stop))
            .expect("spawn clipboard worker thread");

        Self {
            jobs: jobs_tx,
            reads: reads_rx,
            changed: Arc::new(AtomicBool::new(false)),
            watch: None,
            stop,
        }
    }

    /// Start watching the clipboard. Change notifications set a flag polled via
    /// [`clipboard_changed`](Self::clipboard_changed). Safe to call once; a
    /// second call is a no-op.
    pub fn start_watch(&mut self) {
        if self.watch.is_some() {
            return;
        }
        match clipboard_core::watch(Selection::Clipboard) {
            Ok(mut handle) => {
                // Bridge the platform's change channel onto our atomic flag, so
                // the async module polls a bool instead of awaiting a channel it
                // can't hold across a `Send` boundary cheaply.
                let changed = self.changed.clone();
                let stop = self.stop.clone();
                // Take the receiver out to move into the bridge thread; keep the
                // stop guard (`_stop`) in `watch` so the platform thread lives.
                let mut rx = std::mem::replace(
                    &mut handle.changes,
                    tokio::sync::mpsc::unbounded_channel().1,
                );
                self.watch = Some(handle);
                std::thread::Builder::new()
                    .name("clipboard-watch-bridge".into())
                    .spawn(move || {
                        while !stop.load(Ordering::Relaxed) {
                            match rx.blocking_recv() {
                                Some(_) => changed.store(true, Ordering::Relaxed),
                                None => break,
                            }
                        }
                    })
                    .expect("spawn clipboard watch bridge");
                // Offer the content already on the clipboard at session start,
                // not only future copies: set the flag once so the first pump
                // reads and offers what's there.
                self.changed.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot watch clipboard; sharing local copies disabled");
            }
        }
    }

    /// Whether the clipboard has changed since the last poll. Clears the flag.
    pub fn clipboard_changed(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }

    /// Read the current clipboard's formats and sizes (no payloads). Blocks
    /// briefly on the worker; the platform read itself is fast for the metadata.
    pub fn list_formats(&self) -> Result<Vec<ClipboardFormatOffer>, ClipboardError> {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.jobs.send(Job::List(tx)).is_err() {
            return Err(ClipboardError::Backend("clipboard worker gone".into()));
        }
        rx.recv()
            .unwrap_or_else(|_| Err(ClipboardError::Backend("clipboard worker gone".into())))
    }

    /// Ask the worker to load a representation's bytes. Non-blocking: the reply
    /// arrives later via [`next_read_reply`](Self::next_read_reply).
    pub fn request_read(&self, generation: ClipboardGeneration, format: ClipboardFormat) {
        let _ = self.jobs.send(Job::Read { generation, format });
    }

    /// A finished read, if one is ready. Non-blocking.
    pub fn next_read_reply(&self) -> Option<ReadReply> {
        self.reads.try_recv().ok()
    }

    /// Write received content onto the local clipboard. Fire-and-forget: the
    /// worker's `write` detaches a serving thread of its own.
    pub fn write(&self, content: ClipboardContent) {
        let _ = self.jobs.send(Job::Write(content));
    }

    /// Whether the current clipboard is the received copy we most recently
    /// wrote. The write and this check use the same FIFO worker, so a change
    /// notification cannot make the check overtake its corresponding write.
    pub fn matches(&self, content: &ClipboardContent) -> Result<bool, ClipboardError> {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.jobs.send(Job::Matches(content.clone(), tx)).is_err() {
            return Err(ClipboardError::Backend("clipboard worker gone".into()));
        }
        rx.recv()
            .unwrap_or_else(|_| Err(ClipboardError::Backend("clipboard worker gone".into())))
    }

    /// Stop the worker and watch threads.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.watch = None;
    }
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The worker thread body: open the clipboard on first use, then serve jobs.
fn run_worker(
    jobs: std::sync::mpsc::Receiver<Job>,
    reads: std::sync::mpsc::Sender<ReadReply>,
    stop: Arc<AtomicBool>,
) {
    // Opened lazily and cached: a headless host errors here, and we don't want
    // that to kill the session — jobs simply fail until (if ever) it works.
    let mut clipboard: Option<Box<dyn Clipboard>> = None;

    let open = |cb: &mut Option<Box<dyn Clipboard>>| -> Result<(), ClipboardError> {
        if cb.is_none() {
            *cb = Some(clipboard_core::open()?);
        }
        Ok(())
    };

    while !stop.load(Ordering::Relaxed) {
        let job = match jobs.recv() {
            Ok(j) => j,
            Err(_) => break, // handle dropped
        };

        match job {
            Job::List(reply) => {
                let result = match open(&mut clipboard) {
                    Ok(()) => {
                        clipboard
                            .as_mut()
                            .unwrap()
                            .read(Selection::Clipboard)
                            .map(|content| {
                                content
                                    .entries
                                    .iter()
                                    .map(|e| ClipboardFormatOffer {
                                        format: e.format.clone(),
                                        size: e.size,
                                    })
                                    .collect()
                            })
                    }
                    Err(e) => Err(e),
                };
                let _ = reply.send(result);
            }
            Job::Read { generation, format } => {
                let reply = match open(&mut clipboard) {
                    Ok(()) => match clipboard
                        .as_mut()
                        .unwrap()
                        .load(Selection::Clipboard, &format)
                    {
                        Ok(bytes) => ReadReply::Ok {
                            generation,
                            format,
                            bytes,
                        },
                        Err(error) => ReadReply::Failed {
                            generation,
                            format,
                            error,
                        },
                    },
                    Err(error) => ReadReply::Failed {
                        generation,
                        format,
                        error,
                    },
                };
                let _ = reads.send(reply);
            }
            Job::Write(content) => {
                if let Err(e) = open(&mut clipboard) {
                    tracing::warn!(error = %e, "cannot open clipboard to write received content");
                    continue;
                }
                if let Err(e) = clipboard
                    .as_mut()
                    .unwrap()
                    .write(Selection::Clipboard, &content)
                {
                    tracing::warn!(error = %e, "writing received clipboard content failed");
                }
            }
            Job::Matches(content, reply) => {
                let result = match open(&mut clipboard) {
                    Ok(()) => clipboard_matches(clipboard.as_mut().unwrap().as_mut(), &content),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
        }
    }
    tracing::debug!("clipboard worker thread ended");
}

/// Compare a received, fully-loaded copy with the current platform clipboard.
///
/// Comparing only formats or sizes is insufficient: the ping-pong in question
/// has the same one text format and the same size on every iteration. Requiring
/// the same format set and bytes also means a genuine local copy wins even if
/// it arrives while the remote write notification is still in flight.
fn clipboard_matches(
    clipboard: &mut dyn Clipboard,
    expected: &ClipboardContent,
) -> Result<bool, ClipboardError> {
    if expected.is_empty() || expected.entries.iter().any(|entry| !entry.is_loaded()) {
        return Ok(false);
    }

    let current = clipboard.read(Selection::Clipboard)?;
    if current.entries.len() != expected.entries.len()
        || current
            .entries
            .iter()
            .any(|entry| !expected.has(&entry.format))
    {
        return Ok(false);
    }

    for entry in &expected.entries {
        let expected_bytes = entry.bytes.as_deref().expect("loaded above");
        let current_bytes = clipboard.load(Selection::Clipboard, &entry.format)?;
        if current_bytes != expected_bytes {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipboard_core::ClipboardEntry;

    struct MemoryClipboard(ClipboardContent);

    impl Clipboard for MemoryClipboard {
        fn read(&mut self, _selection: Selection) -> Result<ClipboardContent, ClipboardError> {
            Ok(self.0.clone())
        }

        fn load(
            &mut self,
            _selection: Selection,
            format: &ClipboardFormat,
        ) -> Result<Vec<u8>, ClipboardError> {
            self.0
                .bytes(format)
                .map(ToOwned::to_owned)
                .ok_or_else(|| ClipboardError::FormatUnavailable(format.mime().into()))
        }

        fn write(
            &mut self,
            _selection: Selection,
            content: &ClipboardContent,
        ) -> Result<(), ClipboardError> {
            self.0 = content.clone();
            Ok(())
        }
    }

    #[test]
    fn received_copy_matches_its_own_clipboard_write() {
        let expected = ClipboardContent::text("same bytes");
        let mut clipboard = MemoryClipboard(expected.clone());
        assert!(clipboard_matches(&mut clipboard, &expected).unwrap());
    }

    #[test]
    fn same_format_and_size_with_different_bytes_is_not_an_echo() {
        let expected = ClipboardContent::text("first");
        let mut clipboard = MemoryClipboard(ClipboardContent::text("other"));
        assert!(!clipboard_matches(&mut clipboard, &expected).unwrap());
    }

    #[test]
    fn richer_local_copy_with_the_same_plain_text_is_not_an_echo() {
        let expected = ClipboardContent::text("hello");
        let mut local = expected.clone();
        local.insert(ClipboardEntry::loaded(
            ClipboardFormat::Html,
            b"<b>hello</b>".to_vec(),
        ));
        let mut clipboard = MemoryClipboard(local);
        assert!(!clipboard_matches(&mut clipboard, &expected).unwrap());
    }
}
