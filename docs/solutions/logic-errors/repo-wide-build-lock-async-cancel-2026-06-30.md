---
title: "Repo-wide exclusive build lock with async-cancelable wait"
date: 2026-06-30
category: logic-errors
problem_type: logic_error
component: luchta-cli/build_lock
root_cause: "fd-lock v4's RwLockWriteGuard borrows the RwLock, creating self-referential struct ownership; blocking flock cannot be cancelled mid-wait"
resolution_type: code_fix
severity: medium
tags:
  - concurrency
  - flock
  - file-lock
  - self-referential
  - unsafe
  - async-cancel
  - cross-process
  - inode-locking
plan_ref: luchta-build-lock
---

## Problem

Implementing a repo-wide exclusive build lock for luchta CLI required: (1) cross-process mutual exclusion using `flock`, (2) async-cancelable wait so Ctrl+C aborts contention without leaking threads, and (3) integration with fd-lock v4's guard API which borrows the `RwLock<File>`. Holding both the owned `RwLock<File>` and its guard in one struct is self-referential — safe Rust cannot express this ownership pattern directly.

## Symptoms

- Direct use of `spawn_blocking(lock_exclusive)` leaks a permanently-blocked OS thread when user hits Ctrl+C during contention — the blocking `flock` syscall cannot be cancelled mid-wait.
- fd-lock v4's `try_write(&mut self) -> io::Result<RwLockWriteGuard<'_, File>>` returns a guard that borrows the `RwLock<File>`, making it impossible to store both in a struct without self-referential ownership.
- In-process unit tests for contention passed, but cross-process contention behavior differed (fcntl can be re-entrant).

```text
// Rejected approach — leaks thread on Ctrl+C:
let lock = spawn_blocking(|| file.lock_exclusive()).await;
// If user hits Ctrl+C here, the spawned thread is stuck forever in flock()
```

## Investigation Steps

1. Evaluated `fd-lock` v4 API: `RwLock::new(file)` creates lock, `try_write()` returns guard that borrows `&mut RwLock<File>`. Guard's lifetime tied to lock reference.

2. Explored self-referential patterns: `ouroborous`, `self_cell` crates; boxed + transmute; manual unsafe with leak-then-reclaim.

3. Considered `spawn_blocking(lock_exclusive)` for wait — discovered it leaks a blocked thread on cancellation: blocking `flock` syscall cannot be interrupted.

4. Verified flock semantics: guards inode, not pathname. Unlinking lock file allows different-inode file at same path, bypassing mutual exclusion.

5. Designed poll-based `try_write()` loop with `tokio::select!` racing `tokio::signal::ctrl_c()` — polling approach allows cancellation.

6. Determined in-process `fcntl` locks can be re-entrant; real subprocesses needed for accurate cross-process contention tests.

## Root Cause

**Thread leak on cancellation**: A blocking `flock` call inside `spawn_blocking` cannot be cancelled by tokio's runtime cancel mechanism. The OS thread executing `flock` block until lock is acquired, even if the tokio task is cancelled. Ctrl+C triggers task cancellation but the OS thread remains blocked indefinitely.

**Self-referential guard**: fd-lock v4's `RwLockWriteGuard<'_, File>` borrows the `RwLock<File>`. A struct that owns the `RwLock<File>` and holds its guard contains a self-reference — the guard's lifetime is tied to a borrow of a field in the same struct. Safe Rust's ownership model cannot express this.

**WouldBlock error variant**: `try_write()` returns `io::ErrorKind::WouldBlock` on contention, not a distinct `TryLockError` enum. Retry loop needed to handle this as non-fatal.

## Solution

### Async-cancelable polling wait

Poll `try_write()` in a loop, racing against Ctrl+C signal:

```rust
const ACQUIRE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

async fn acquire_with_cancel<F>(cache_dir: &Path, cancel: F) -> miette::Result<Option<BuildLock>>
where
    F: Future<Output = io::Result<()>>,
{
    let lock_path = cache_dir.join("build.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;

    // First attempt — fast path
    let mut boxed = Box::new(RwLock::new(file));
    match try_build(boxed)? {
        Ok(build_lock) => return Ok(Some(build_lock)),
        Err(returned_box) => boxed = returned_box,
    }

    // Contention path — poll with cancellation
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
                result.wrap_err("failed to install Ctrl-C handler")?;
                return Ok(None); // Cancelled — caller exits gracefully
            }
        }
    }
}
```

### Self-referential guard with leak-then-reclaim

Store guard as `Option<RwLockWriteGuard<'static, File>>` with raw pointer to leaked `RwLock<File>`:

```rust
pub struct BuildLock {
    guard: Option<RwLockWriteGuard<'static, File>>,
    lock_ptr: *mut RwLock<File>,
}

// SAFETY: Heap allocation has stable address; guard's 'static reference remains valid.
// BuildLock owns exclusively — no other references to the leaked allocation exist.
unsafe impl Send for BuildLock {}

fn try_build(boxed: Box<RwLock<File>>) -> miette::Result<Result<BuildLock, Box<RwLock<File>>>> {
    let ptr = Box::into_raw(boxed);

    match unsafe { &mut *ptr }.try_write() {
        Ok(guard) => Ok(Ok(BuildLock {
            guard: Some(guard),
            lock_ptr: ptr,
        })),
        Err(error) => {
            // Reclaim Box on WouldBlock or error — no leak
            let boxed = unsafe { Box::from_raw(ptr) };

            if error.kind() == io::ErrorKind::WouldBlock {
                Ok(Err(boxed)) // Return Box for retry
            } else {
                Err(error).into_diagnostic()
            }
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        self.guard.take(); // Release flock first

        // Reclaim leaked allocation after guard dropped
        unsafe { drop(Box::from_raw(self.lock_ptr)) };
    }
}
```

### Never unlink the lock file

```rust
// In Drop — intentionally do NOT unlink <cache_dir>/build.lock
// flock guards inode, not pathname; unlinking allows different-inode bypass
// OS auto-releases fd on process death for crash safety
```

### Deterministic cross-process contention test

```rust
#[test]
fn concurrent_runs_wait_for_build_lock_across_processes() {
    let temp = TempDir::new().expect("create temp dir");
    let (hold_path, release_path, finished_path) = build_fixture(&temp);
    let cache_dir = temp.child("cache");
    fs::create_dir_all(cache_dir.path()).expect("create cache dir");

    // Process A — holds lock
    let first = Command::new(binary_path())
        .arg("run").arg("build")
        .env("LUCHTA_CACHE_DIR", cache_dir.path())
        .spawn()
        .expect("spawn first");

    // Wait for marker — proves A reached worker phase and holds lock
    wait_for(Duration::from_secs(30), || hold_path.exists(), "first to hold");

    // Process B — should block on contention
    let second_stderr = temp.child("second.stderr");
    let mut second = Command::new(binary_path())
        .arg("run").arg("build")
        .env("LUCHTA_CACHE_DIR", cache_dir.path())
        .stderr(fs::File::create(second_stderr.path()).expect("create stderr"))
        .spawn()
        .expect("spawn second");

    // Assert B prints waiting message and stays blocked
    wait_for_waiting_message(second_stderr.path());
    assert!(second.try_wait().expect("poll").is_none(), "B still blocked");

    // Release A — B should proceed
    fs::write(&release_path, "release").expect("release");
    let first_output = first.wait_with_output().expect("wait first");
    assert!(first_output.status.success(), "A succeeds");

    let second_status = second.wait().expect("wait second");
    assert!(second_status.success(), "B succeeds");
}
```

Test uses shared `LUCHTA_CACHE_DIR` env, marker files for readiness gating, and stderr redirect for message assertion — no long sleeps.

## Why This Works

**Polling + select**: `try_write()` with 100ms polling allows `tokio::select!` to check Ctrl+C on each iteration. Cancellation is immediate (within one poll interval). No OS thread leaked.

**Heap allocation stability**: `Box::into_raw` leaks the `RwLock<File>` to a stable heap address. The `'static` reference in the guard is valid as long as the allocation exists. Drop order (guard first, then Box reclamation) ensures flock is released before memory freed.

**WouldBlock as retry signal**: fd-lock surfaces contention as `io::ErrorKind::WouldBlock`, not a distinct enum. The `try_build` helper returns `Result<BuildLock, Box<RwLock<File>>>` — the `Err` variant returns the Box for retry without allocation churn.

**Cross-process correctness**: Real subprocesses (not in-process fcntl) prove true cross-process contention. Marker files provide deterministic ordering without timing-based assertions.

## Prevention Strategies

**Test Cases:**
- Uncontended acquire succeeds immediately
- Second acquire blocks while first holds lock
- Ctrl+C during contention returns `Ok(None)` — no leaked threads
- Lock file persists after guard drop — never unlinked
- Cross-process contention: Process B blocks on Process A's lock, completes after A releases

**Best Practices:**
- Never use `spawn_blocking` with non-cancelable blocking syscalls (flock, mutex, I/O) when cancellation is required
- For file locks: polling `try_lock()` + `select!` on cancel signal is the cancellable pattern
- Never unlink lock files for flock-based mutual exclusion — guards inode, not path
- Test cross-process contention with real subprocesses, not in-process locking (fcntl can be re-entrant)
- Use marker files and readiness gating for deterministic concurrent test ordering

**Code Review Checklist:**
- [ ] Is async wait cancellable without leaking threads?
- [ ] Is lock file never deleted?
- [ ] Are cross-process contention tests using real subprocesses?
- [ ] Are marker files used for readiness instead of fixed sleeps?
- [ ] If using unsafe for self-referential ownership, is Drop order correct (guard before allocation)?

## Related Issues

- **GitHub:** [dobesv/luchta#8](https://github.com/dobesv/luchta/issues/8) — Prevent concurrent conflicting builds
- **Related Solution:** [logic-errors/shared-build-cache-validate-on-restore-2026-06-17.md](../logic-errors/shared-build-cache-validate-on-restore-2026-06-17.md) — flock guards inode not pathname; never unlink lock files
- **Related Solution:** [integration-issues/resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — Exclusive lock ownership under concurrency
- **Related Solution:** [performance-issues/remote-cache-parallel-sync-nested-runtime-2026-06-24.md](../performance-issues/remote-cache-parallel-sync-nested-runtime-2026-06-24.md) — Do not `block_on` inside existing tokio runtime

## Additional Learnings

**fd-lock v4 API**: `RwLock<File>` wrapper with `try_write()` returning `io::Result<RwLockWriteGuard<'_, File>>`. WouldBlock surfaced as `io::ErrorKind::WouldBlock`. Contention must be handled via retry loop.

**fs2 dep stale**: Workspace has `fs2 = "0.4.3"` at root Cargo.toml — do NOT use; `fd-lock` is maintained and correct choice.

**Binary crate module declaration**: `luchta-cli` has no `lib.rs`; modules declared in `main.rs`. Add `mod build_lock;` there.

**Lock file path**: `LUCHTA_CACHE_DIR` env override enables test isolation. Lock file is `<cache_dir>/build.lock` (not `.luchta/build.lock`).

**Watch mode semantics**: Acquire before `run_cycle_with_status()`, release before `wait_for_pending_or_shutdown()` idle wait. Idle watch lets `luchta run` proceed; mid-build watch blocks.
