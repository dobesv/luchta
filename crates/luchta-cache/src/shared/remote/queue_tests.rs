use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

fn test_remote(timeout: Duration, concurrency: usize) -> RemoteSync {
    RemoteSync::new(
        Arc::new(RcloneRcd::with_concurrency_limit(timeout, concurrency).unwrap()),
        ":local:/tmp/nonexistent-remote",
        3,
    )
}

fn delayed_job(duration: Duration, completed: Arc<AtomicUsize>) -> PushMsg {
    PushMsg::TestDelay {
        duration,
        active: Arc::new(AtomicUsize::new(0)),
        max_active: Arc::new(AtomicUsize::new(0)),
        completed,
    }
}

#[test]
fn saturated_push_queue_drops_without_blocking() {
    let remote = test_remote(Duration::from_secs(1), 1);
    let push = OwnedEntryArtifacts {
        paths: Arc::new(SharedCachePaths {
            root: PathBuf::from("/tmp/luchta-saturated-test"),
            blobs_dir: PathBuf::from("/tmp/luchta-saturated-test/blobs"),
            cache_files_dir: PathBuf::from("/tmp/luchta-saturated-test/cache-files"),
            snapshots_dir: PathBuf::from("/tmp/luchta-saturated-test/snapshots"),
            entries_dir: PathBuf::from("/tmp/luchta-saturated-test/entries"),
        }),
        outputs_hash: [1; 32],
        input_key: [2; 32],
        presence: ArtifactPresence::all(),
    };
    remote
        .push_runtime
        .depth
        .store(remote.push_runtime.capacity, Ordering::Release);
    let started = Instant::now();
    remote.enqueue_entry_artifacts(push);
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(remote.state.stats.queue_drops.load(Ordering::Acquire), 1);
    remote.push_runtime.depth.store(0, Ordering::Release);
}

#[test]
fn push_workers_run_artifacts_with_configured_concurrency() {
    let remote = test_remote(Duration::from_secs(2), 4);
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    for _ in 0..40 {
        assert!(remote.enqueue_push(PushMsg::TestDelay {
            duration: Duration::from_millis(5),
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
            completed: Arc::clone(&completed),
        }));
    }

    remote.drain_push_queue();

    assert_eq!(completed.load(Ordering::Acquire), 40);
    let peak = max_active.load(Ordering::Acquire);
    assert!(
        peak > 1,
        "workers must run artifact jobs in parallel, observed peak {peak}"
    );
    assert!(
        peak <= 4,
        "workers must not exceed configured concurrency, observed peak {peak}"
    );
}

#[test]
fn flush_waits_for_out_of_order_parallel_jobs() {
    let remote = test_remote(Duration::from_secs(2), 2);
    let completed = Arc::new(AtomicUsize::new(0));
    assert!(remote.enqueue_push(delayed_job(
        Duration::from_millis(100),
        Arc::clone(&completed),
    )));
    assert!(remote.enqueue_push(delayed_job(
        Duration::from_millis(5),
        Arc::clone(&completed),
    )));

    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    assert!(remote.enqueue_push(PushMsg::Flush(ack_tx)));
    ack_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("flush should run after both parallel jobs complete");

    assert_eq!(
        completed.load(Ordering::Acquire),
        2,
        "the serialized flush must wait for every preceding job, including one that finishes out of order"
    );
}

#[test]
fn stalled_push_drain_respects_one_total_deadline() {
    let remote = test_remote(Duration::from_millis(40), 1);
    let completed = Arc::new(AtomicUsize::new(0));
    assert!(remote.enqueue_push(delayed_job(
        Duration::from_millis(250),
        Arc::clone(&completed),
    )));

    let started = Instant::now();
    remote.drain_push_queue();

    assert!(started.elapsed() < Duration::from_millis(150));
    assert!(remote.push_runtime.cancelled.load(Ordering::Acquire));
    assert_eq!(completed.load(Ordering::Acquire), 0);
}

#[test]
fn explicit_discard_is_immediate_without_counting_as_timeout() {
    let remote = test_remote(Duration::from_secs(1), 1);
    assert!(remote.enqueue_push(delayed_job(
        Duration::from_millis(250),
        Arc::new(AtomicUsize::new(0)),
    )));
    let started = Instant::now();

    remote.discard_push_queue();

    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(remote.state.timeout_count.load(Ordering::Acquire), 0);
}
