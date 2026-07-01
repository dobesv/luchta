use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io,
    path::Path,
    time::Duration,
};

use fd_lock::{RwLock, RwLockWriteGuard};
use miette::{Context, IntoDiagnostic};

/// Poll interval used while waiting for a contended build lock to be released.
const ACQUIRE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

pub struct BuildLock {
    guard: Option<RwLockWriteGuard<'static, File>>,
    lock_ptr: *mut RwLock<File>,
}

// SAFETY: the guard and the leaked `RwLock<File>` allocation are owned exclusively by this
// BuildLock — no other reference to the leaked allocation exists. The allocation is on the heap
// (via Box), so its address stays stable even when the BuildLock struct is moved across threads,
// keeping the guard's internal `'static` reference valid.
unsafe impl Send for BuildLock {}

impl Drop for BuildLock {
    fn drop(&mut self) {
        self.guard.take();

        // SAFETY: lock_ptr came from Box::into_raw and is reclaimed exactly once here, after
        // guard has been dropped and released flock. We intentionally do NOT unlink
        // <cache_dir>/build.lock — flock guards inode, not pathname; unlinking would let a
        // different-inode file at same path bypass mutual exclusion. OS auto-releases fd on
        // process death.
        unsafe { drop(Box::from_raw(self.lock_ptr)) };
    }
}

pub async fn acquire(cache_dir: &Path) -> miette::Result<Option<BuildLock>> {
    acquire_with_cancel(cache_dir, tokio::signal::ctrl_c()).await
}

async fn acquire_with_cancel<F>(cache_dir: &Path, cancel: F) -> miette::Result<Option<BuildLock>>
where
    F: Future<Output = io::Result<()>>,
{
    fs::create_dir_all(cache_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create cache dir {}", cache_dir.display()))?;

    let lock_path = cache_dir.join("build.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to open build lock file {}", lock_path.display()))?;

    try_acquire_with_cancel(file, cancel).await
}

async fn try_acquire_with_cancel<F>(file: File, cancel: F) -> miette::Result<Option<BuildLock>>
where
    F: Future<Output = io::Result<()>>,
{
    let mut boxed = Box::new(RwLock::new(file));

    match try_build(boxed)? {
        Ok(build_lock) => return Ok(Some(build_lock)),
        Err(returned_box) => boxed = returned_box,
    }

    eprintln!("Waiting for concurrent build ...");
    tokio::pin!(cancel);

    loop {
        tokio::select! {
            _ = tokio::time::sleep(ACQUIRE_RETRY_INTERVAL) => {
                match try_build(boxed)? {
                    Ok(build_lock) => return Ok(Some(build_lock)),
                    Err(returned_box) => boxed = returned_box,
                }
            }
            result = &mut cancel => {
                result
                    .into_diagnostic()
                    .wrap_err("failed to install Ctrl-C handler")?;
                return Ok(None);
            }
        }
    }
}

fn try_build(boxed: Box<RwLock<File>>) -> miette::Result<Result<BuildLock, Box<RwLock<File>>>> {
    let ptr = Box::into_raw(boxed);

    // SAFETY: ptr came from Box::into_raw and is exclusively owned by this function until either
    // wrapped into BuildLock on success or reclaimed with Box::from_raw on retry/error.
    match unsafe { &mut *ptr }.try_write() {
        Ok(guard) => Ok(Ok(BuildLock {
            guard: Some(guard),
            lock_ptr: ptr,
        })),
        Err(error) => {
            // SAFETY: ptr came from Box::into_raw above and has not been reclaimed yet.
            let boxed = unsafe { Box::from_raw(ptr) };

            if error.kind() == io::ErrorKind::WouldBlock {
                Ok(Err(boxed))
            } else {
                Err(error)
                    .into_diagnostic()
                    .wrap_err("failed to acquire build lock")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future, sync::Arc};

    use tempfile::TempDir;
    use tokio::{
        sync::Notify,
        task::JoinHandle,
        time::{self, Duration},
    };

    use super::{acquire, acquire_with_cancel};

    fn cache_dir(temp: &TempDir) -> std::path::PathBuf {
        temp.path().join("cache")
    }

    #[tokio::test]
    async fn acquire_returns_guard_without_contention() {
        let temp = TempDir::new().expect("create temp dir");

        let lock = acquire(&cache_dir(&temp))
            .await
            .expect("acquire build lock");

        assert!(lock.is_some(), "expected uncontended acquire to succeed");
    }

    #[tokio::test]
    async fn second_acquire_waits_while_first_guard_is_held() {
        let temp = TempDir::new().expect("create temp dir");
        let cache_dir = cache_dir(&temp);
        let _first = acquire(&cache_dir)
            .await
            .expect("acquire first build lock")
            .expect("first build lock present");

        let waiter_started = Arc::new(Notify::new());
        let waiter_started_clone = Arc::clone(&waiter_started);
        let cache_dir_clone = cache_dir.clone();
        let waiter: JoinHandle<miette::Result<Option<super::BuildLock>>> =
            tokio::spawn(async move {
                waiter_started_clone.notify_one();
                acquire_with_cancel(&cache_dir_clone, future::pending()).await
            });

        waiter_started.notified().await;
        time::sleep(Duration::from_millis(250)).await;
        assert!(
            !waiter.is_finished(),
            "second acquire should still be waiting while first guard is held"
        );

        waiter.abort();
        let join_result = waiter.await;
        assert!(
            join_result.is_err(),
            "aborted waiter task should cancel cleanly"
        );
    }

    #[tokio::test]
    async fn acquire_returns_none_when_cancel_is_ready_during_contention() {
        let temp = TempDir::new().expect("create temp dir");
        let cache_dir = cache_dir(&temp);
        let _first = acquire(&cache_dir)
            .await
            .expect("acquire first build lock")
            .expect("first build lock present");

        let result = time::timeout(
            Duration::from_secs(1),
            acquire_with_cancel(&cache_dir, future::ready(Ok(()))),
        )
        .await
        .expect("cancelled acquire should finish promptly")
        .expect("cancelled acquire should not error");

        assert!(result.is_none(), "cancelled acquire should return None");
    }

    #[tokio::test]
    async fn dropping_guard_keeps_lock_file_on_disk() {
        let temp = TempDir::new().expect("create temp dir");
        let cache_dir = cache_dir(&temp);
        let lock_path = cache_dir.join("build.lock");

        {
            let _lock = acquire(&cache_dir)
                .await
                .expect("acquire build lock")
                .expect("build lock present");
            assert!(
                lock_path.exists(),
                "lock file should exist while guard is held"
            );
        }

        assert!(
            lock_path.exists(),
            "lock file should persist after guard drop"
        );
    }
}
