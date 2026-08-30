use std::sync::{Condvar, Mutex};

/// Admission for complete logical remote operations.
///
/// The permit stays held while rclone executes and is polled, not merely while
/// the request is submitted, so the configured limit reflects actual backend
/// load.
///
/// This limiter is synchronous by design: callers are blocking restore and
/// upload threads, so a Tokio semaphore would require runtime re-entry merely
/// to wait for admission. Every remote operation has the same logical weight.
#[derive(Debug)]
pub(super) struct RemoteOperationLimiter {
    state: Mutex<(usize, usize)>,
    ready: Condvar,
}

impl RemoteOperationLimiter {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new((limit, 0)),
            ready: Condvar::new(),
        }
    }

    pub(super) fn acquire(&self) -> RemoteOperationPermit<'_> {
        let mut state = self
            .state
            .lock()
            .expect("remote operation limiter poisoned");
        while state.1 >= state.0 {
            state = self
                .ready
                .wait(state)
                .expect("remote operation limiter poisoned");
        }
        state.1 += 1;
        RemoteOperationPermit { limiter: self }
    }
}

pub(super) struct RemoteOperationPermit<'a> {
    limiter: &'a RemoteOperationLimiter,
}

impl Drop for RemoteOperationPermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .limiter
            .state
            .lock()
            .expect("remote operation limiter poisoned");
        state.1 -= 1;
        self.limiter.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn bounds_and_parallelizes_many_requests() {
        let limiter = Arc::new(RemoteOperationLimiter::new(8));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let threads = (0..100)
            .map(|_| {
                let limiter = Arc::clone(&limiter);
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                std::thread::spawn(move || {
                    let _permit = limiter.acquire();
                    let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                    max_active.fetch_max(current, Ordering::AcqRel);
                    std::thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::AcqRel);
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }

        let observed = max_active.load(Ordering::Acquire);
        assert!(observed > 1, "requests should overlap");
        assert!(observed <= 8, "logical concurrency limit must be respected");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "100 delayed requests should not take serial latency"
        );
    }
}
