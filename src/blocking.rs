//! `CpuPool` (rayon, 16 MiB stacks), `catch_unwind`, permit-in-job, watchdog.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::admission::JobPermit;

/// Worker stack size. A stack overflow aborts the process and cannot be
/// caught, so deep-recursion documents need real headroom.
pub const WORKER_STACK: usize = 16 * 1024 * 1024;

/// The job panicked; the engine hit an internal error on this input.
#[derive(Debug)]
pub struct JobPanicked;

pub struct CpuPool {
    pool: rayon::ThreadPool,
    watchdog: Arc<Watchdog>,
}

impl CpuPool {
    pub fn new(threads: usize, watchdog: Arc<Watchdog>) -> Self {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .stack_size(WORKER_STACK)
            .thread_name(|i| format!("cpu-{i}"))
            .build()
            .expect("failed to build CPU pool");
        Self { pool, watchdog }
    }

    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Run `f` on the pool. The permit moves into the job and is released
    /// only when the job really ends, whatever happened to the request.
    pub fn spawn<T, F>(&self, permit: JobPermit, deadline: Duration, f: F) -> oneshot::Receiver<Result<T, JobPanicked>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let watchdog = self.watchdog.clone();
        self.pool.spawn(move || {
            let _watch = watchdog.register(deadline);
            let out = catch_unwind(AssertUnwindSafe(f)).map_err(|_| JobPanicked);
            drop(_watch);
            drop(permit);
            let _ = tx.send(out);
        });
        rx
    }
}

/// Flags `/healthz` as failing when any job runs longer than 10x its
/// deadline: rayon work cannot be cancelled, so a wedged job is a reason for
/// the orchestrator to restart the container.
#[derive(Default)]
pub struct Watchdog {
    next_id: AtomicU64,
    jobs: Mutex<HashMap<u64, (Instant, Duration)>>,
    tripped: AtomicBool,
}

pub struct WatchGuard {
    id: u64,
    dog: Arc<Watchdog>,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        if let Ok(mut jobs) = self.dog.jobs.lock() {
            jobs.remove(&self.id);
        }
    }
}

impl Watchdog {
    pub const FACTOR: u32 = 10;

    pub fn register(self: &Arc<Self>, deadline: Duration) -> WatchGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.insert(id, (Instant::now(), deadline));
        }
        WatchGuard { id, dog: self.clone() }
    }

    /// Returns true if the watchdog is (now) tripped.
    pub fn check(&self) -> bool {
        if self.tripped.load(Ordering::Relaxed) {
            return true;
        }
        let wedged = self
            .jobs
            .lock()
            .map(|jobs| jobs.values().any(|(start, deadline)| start.elapsed() > *deadline * Self::FACTOR))
            .unwrap_or(true);
        if wedged {
            tracing::error!("watchdog: a job exceeded {}x its deadline; reporting unhealthy", Self::FACTOR);
            self.tripped.store(true, Ordering::Relaxed);
        }
        wedged
    }

    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Relaxed)
    }

    pub fn running(&self) -> usize {
        self.jobs.lock().map(|j| j.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_trips_on_wedged_job() {
        let dog = Arc::new(Watchdog::default());
        let g = dog.register(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(20));
        assert!(dog.check());
        drop(g);
        assert!(dog.is_tripped(), "trip is sticky");
    }

    #[test]
    fn watchdog_ok_when_jobs_finish() {
        let dog = Arc::new(Watchdog::default());
        drop(dog.register(Duration::from_secs(1)));
        assert!(!dog.check());
        assert_eq!(dog.running(), 0);
    }
}
