use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use serde::{Deserialize, Serialize};

/// Absolute worker-level progress counters for one running task.
///
/// `completed` includes every terminal item, including the `skipped` subset.
/// Each protocol message replaces the previous snapshot in full.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgress {
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub skipped: u64,
    #[serde(default)]
    pub running: u64,
    #[serde(default)]
    pub pending: u64,
}

impl TaskProgress {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.completed == 0 && self.skipped == 0 && self.running == 0 && self.pending == 0
    }
}

/// Diagnostics used while processing a worker's items on blocking threads.
#[derive(Debug, Clone, Copy)]
pub struct ParallelProgress {
    pub(crate) worker_name: &'static str,
    pub(crate) panic_message: &'static str,
}

impl ParallelProgress {
    #[must_use]
    pub const fn new(worker_name: &'static str, panic_message: &'static str) -> Self {
        Self {
            worker_name,
            panic_message,
        }
    }
}

/// Thread-safe counters for work processed at discrete item boundaries.
///
/// Clone this tracker into blocking worker threads. Call [`Self::start_item`]
/// immediately before processing each item; dropping the returned guard marks
/// that item completed, including error paths and unwinding.
#[derive(Debug, Clone)]
pub struct ItemProgress {
    state: Arc<ItemProgressState>,
}

#[derive(Debug)]
struct ItemProgressState {
    revision: AtomicU64,
    completed: AtomicU64,
    skipped: AtomicU64,
    running: AtomicU64,
    pending: AtomicU64,
}

impl ItemProgress {
    #[must_use]
    pub fn new(total: usize) -> Self {
        Self {
            state: Arc::new(ItemProgressState {
                revision: AtomicU64::new(0),
                completed: AtomicU64::new(0),
                skipped: AtomicU64::new(0),
                running: AtomicU64::new(0),
                pending: AtomicU64::new(u64::try_from(total).unwrap_or(u64::MAX)),
            }),
        }
    }

    /// Move one pending item into the running state.
    #[must_use]
    pub fn start_item(&self) -> ItemProgressGuard {
        let revision = self.state.begin_update();
        let pending = self.state.pending.load(Ordering::Relaxed);
        let had_pending = pending > 0;
        if had_pending {
            self.state.pending.store(pending - 1, Ordering::Relaxed);
            self.state.running.fetch_add(1, Ordering::Relaxed);
        }
        self.state.end_update(revision);
        ItemProgressGuard {
            progress: self.clone(),
            active: had_pending,
            skipped: false,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> TaskProgress {
        loop {
            let revision = self.state.revision.load(Ordering::Acquire);
            if revision % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let snapshot = TaskProgress {
                completed: self.state.completed.load(Ordering::Relaxed),
                skipped: self.state.skipped.load(Ordering::Relaxed),
                running: self.state.running.load(Ordering::Relaxed),
                pending: self.state.pending.load(Ordering::Relaxed),
            };
            if self.state.revision.load(Ordering::Acquire) == revision {
                return snapshot;
            }
        }
    }
}

impl ItemProgressState {
    fn begin_update(&self) -> u64 {
        loop {
            let revision = self.revision.load(Ordering::Acquire);
            if revision % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            if self
                .revision
                .compare_exchange_weak(
                    revision,
                    revision.wrapping_add(1),
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return revision;
            }
        }
    }

    fn end_update(&self, revision: u64) {
        self.revision
            .store(revision.wrapping_add(2), Ordering::Release);
    }
}

/// Completion guard for one tracked item.
#[derive(Debug)]
pub struct ItemProgressGuard {
    progress: ItemProgress,
    active: bool,
    skipped: bool,
}

impl ItemProgressGuard {
    /// Mark this item as intentionally bypassed. It remains part of
    /// `completed`; `skipped` is only a displayed subset.
    pub fn skip(mut self) {
        self.skipped = true;
    }
}

impl Drop for ItemProgressGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let revision = self.progress.state.begin_update();
        if self.skipped {
            self.progress.state.skipped.fetch_add(1, Ordering::Relaxed);
        }
        self.progress
            .state
            .completed
            .fetch_add(1, Ordering::Relaxed);
        self.progress.state.running.fetch_sub(1, Ordering::Relaxed);
        self.progress.state.end_update(revision);
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::thread;

    use super::{ItemProgress, TaskProgress};

    #[test]
    fn item_guards_track_completed_and_skipped_subsets() {
        let progress = ItemProgress::new(3);
        let first = progress.start_item();
        assert_eq!(
            progress.snapshot(),
            TaskProgress {
                running: 1,
                pending: 2,
                ..TaskProgress::default()
            }
        );
        drop(first);
        progress.start_item().skip();

        assert_eq!(
            progress.snapshot(),
            TaskProgress {
                completed: 2,
                skipped: 1,
                pending: 1,
                ..TaskProgress::default()
            }
        );
    }

    #[test]
    fn item_guard_completes_during_failure_unwind() {
        let progress = ItemProgress::new(1);
        let failure = catch_unwind(AssertUnwindSafe(|| {
            let _item = progress.start_item();
            panic!("representative item failure");
        }));

        assert!(failure.is_err());
        assert_eq!(
            progress.snapshot(),
            TaskProgress {
                completed: 1,
                ..TaskProgress::default()
            }
        );
    }

    fn complete_items(progress: &ItemProgress, count: u64) {
        for _ in 0..count {
            drop(progress.start_item());
        }
    }

    fn assert_coherent_until_complete(progress: &ItemProgress, total: u64) {
        loop {
            let snapshot = progress.snapshot();
            assert_eq!(
                snapshot.completed + snapshot.running + snapshot.pending,
                total
            );
            if snapshot.completed == total {
                break;
            }
            thread::yield_now();
        }
    }

    #[test]
    fn concurrent_snapshots_never_observe_partial_item_transitions() {
        const TOTAL: u64 = 1_000;
        let progress = ItemProgress::new(TOTAL as usize);

        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| complete_items(&progress, TOTAL / 4));
            }

            assert_coherent_until_complete(&progress, TOTAL);
        });
    }
}
