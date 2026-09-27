//! Threads that do the file work, deliberately kept out of everyone's way.
//!
//! File sharing is the one part of cursio that can saturate a machine. A copy
//! reads the disk as fast as it is allowed to, compresses every chunk, and does
//! it on as many requests as are in flight — while the user is doing something
//! else on both machines. The cursor, the audio stream and whatever else the
//! user is running have to keep their responsiveness through that, and none of
//! them can ask for it: they simply get slower.
//!
//! So the work runs here instead, on a pool that is
//!
//! - **small** — a bounded number of threads, so a transfer can never occupy
//!   every core the way an unbounded [`spawn_blocking`] fan-out can, and
//! - **polite** — each thread lowers its own scheduling priority at startup, so
//!   the OS hands the CPU to ordinary work first and this pool takes what is
//!   left. On an idle machine that is everything; on a busy one the transfer
//!   slows down instead of the foreground.
//!
//! [`spawn_blocking`]: tokio::task::spawn_blocking
//!
//! ## Why not `spawn_blocking`
//!
//! Tokio's blocking pool is shared with every other module and grows to 512
//! threads on demand, so file work would both crowd out other subsystems' short
//! blocking calls and run at normal priority. Lowering the priority of a pooled
//! thread is worse than not doing it at all: the thread goes back to the pool
//! still demoted, and the next module to borrow it inherits the demotion.
//! Owning the threads is what makes the priority ours to set.

use std::sync::{mpsc, Arc, Mutex};

/// One unit of file work.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// A pool of low-priority threads.
///
/// Cloning shares the pool. The threads run until the last handle is dropped,
/// then exit as their current job finishes.
#[derive(Clone)]
pub struct Workers {
    jobs: mpsc::Sender<Job>,
}

impl Workers {
    /// Start a pool sized for this machine.
    pub fn new(name: &'static str) -> Self {
        Self::with_threads(name, worker_count())
    }

    /// Start a pool of exactly `threads` workers. Prefer [`Workers::new`];
    /// this exists for tests, which need a size they can reason about.
    pub fn with_threads(name: &'static str, threads: usize) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        // One receiver behind a mutex is the whole dispatch: a worker holds the
        // lock only long enough to take a job, never while running it.
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..threads.max(1) {
            let rx = Arc::clone(&rx);
            let spawned = std::thread::Builder::new()
                .name(format!("{name}-{i}"))
                .spawn(move || {
                    yield_to_everything_else();
                    loop {
                        // `recv` blocks; the lock is released before the job
                        // runs so the other workers keep taking work.
                        let job = {
                            let guard = rx.lock().unwrap_or_else(|p| p.into_inner());
                            guard.recv()
                        };
                        match job {
                            // Caught, because an unwinding job otherwise takes
                            // its thread with it and the pool never gets it
                            // back: a handful of panics and nothing serves the
                            // peer at all, while every request it sends waits
                            // out its deadline and the link's window stays
                            // shut until a reconnect. A job holds no state of
                            // the pool's, so the next one starts clean.
                            Ok(job) => {
                                if let Err(panic) =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                                {
                                    tracing::warn!(
                                        why = panic_message(&panic),
                                        "a file job panicked; the worker carries on"
                                    );
                                }
                            }
                            // Every sender is gone: the session ended.
                            Err(_) => break,
                        }
                    }
                });
            if let Err(e) = spawned {
                // A machine that cannot spawn threads has larger problems, and
                // the pool still works with the ones that did start.
                tracing::warn!(error = %e, "could not start a file worker thread");
            }
        }
        Workers { jobs }
    }

    /// Run `job` on the pool.
    ///
    /// Dropped silently if every worker is gone, which only happens once the
    /// session is tearing down — the answer would have nowhere to go anyway.
    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.jobs.send(Box::new(job));
    }
}

/// The text a panic was raised with, when it carried any.
pub fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message")
}

/// How many workers to run.
///
/// Sized from the machine rather than fixed: this is disk work, which spends
/// most of its time waiting, so a few threads per core keep a queue on the
/// device without the pool itself becoming the load. The ceiling matters more
/// than the exact figure — the pool exists to be *bounded*, and past a point
/// more threads only make a mechanical disk seek harder and a laptop hotter.
fn worker_count() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores * 2).clamp(4, 16)
}

/// Ask the OS to schedule this thread behind ordinary work.
///
/// Called once per worker at startup. Best-effort everywhere: failing to lower
/// the priority costs politeness, never correctness, so an error is logged at
/// most and the thread carries on.
fn yield_to_everything_else() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // BELOW_NORMAL rather than `THREAD_MODE_BACKGROUND_BEGIN`: background
        // mode also drops the thread's I/O and memory priority to the floor,
        // which on a busy machine stalls a transfer outright rather than merely
        // yielding to the foreground. One notch down still means every normal
        // thread on the system — the user's editor, the browser, our own audio
        // — is served first, while a transfer on an otherwise idle machine runs
        // at full speed.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    #[cfg(target_os = "linux")]
    {
        // `PRIO_PROCESS` with a *thread* id is per-thread on Linux, which is
        // what we want: the rest of cursio must keep its own priority.
        //
        // +5 is the conventional "background but not starved" nice level (make
        // and rsync wrappers use it). It only takes effect under contention.
        const FILE_WORK_NICE: libc::c_int = 5;
        unsafe {
            let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
            libc::setpriority(libc::PRIO_PROCESS, tid, FILE_WORK_NICE);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // `PRIO_DARWIN_THREAD` addresses the calling thread and takes no id —
        // the Linux trick of passing a tid to `PRIO_PROCESS` would nice the
        // whole process here, cursor and all.
        //
        // Not `PRIO_DARWIN_BG`: that is Apple's full background band, which
        // also throttles the thread's disk I/O, and a transfer on an otherwise
        // idle Mac would crawl for no one's benefit. Same reasoning as the
        // Windows arm above.
        const FILE_WORK_NICE: libc::c_int = 5;
        unsafe {
            libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, FILE_WORK_NICE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// The pool must actually run what it is given, from several threads.
    #[test]
    fn every_job_handed_over_gets_run() {
        let workers = Workers::with_threads("test-files", 4);
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..64 {
            let done = Arc::clone(&done);
            workers.spawn(move || {
                done.fetch_add(1, Ordering::SeqCst);
            });
        }
        // Generous: the pool is deliberately low priority, so a loaded test
        // machine may schedule it late.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while done.load(Ordering::SeqCst) < 64 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(done.load(Ordering::SeqCst), 64);
    }

    /// A slow job must not hold up the others: the queue is shared, not
    /// per-thread, so one long read cannot block the requests behind it.
    #[test]
    fn a_slow_job_does_not_block_the_pool() {
        let workers = Workers::with_threads("test-files-slow", 4);
        let (tx, rx) = mpsc::channel();
        let blocker = Arc::new(std::sync::Barrier::new(2));
        {
            let blocker = Arc::clone(&blocker);
            workers.spawn(move || {
                blocker.wait();
            });
        }
        workers.spawn(move || {
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the second job must run while the first is still stuck"
        );
        blocker.wait();
    }

    /// A job that panics must not cost the pool its thread.
    ///
    /// With one worker this is the whole pool: if the panic took the thread,
    /// the job after it would never run.
    #[test]
    fn a_panicking_job_does_not_take_its_worker_with_it() {
        let workers = Workers::with_threads("test-files-panic", 1);
        workers.spawn(|| panic!("a job that fails badly"));
        let (tx, rx) = mpsc::channel();
        workers.spawn(move || {
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the worker must still be there for the next job"
        );
    }

    /// The pool has to stay bounded — being bounded is the point.
    #[test]
    fn the_pool_stays_small_enough_to_share_the_machine() {
        let n = worker_count();
        assert!(n >= 4, "too few to keep a disk queue busy: {n}");
        assert!(n <= 16, "a transfer must not own every core: {n}");
    }
}
