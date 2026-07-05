---
title: "Stay-resident worker process management with tokio JSONL IPC"
date: 2026-06-09
category: integration-issues
problem_type: integration_issue
component: luchta-engine/worker
root_cause: "Concurrency bugs in worker lifecycle management and cross-platform portability gaps"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - process-management
  - concurrency
  - race-condition
  - jsonl-ipc
  - process-group
  - shutdown-timeout
  - cross-platform
  - cfg-gating
plan_ref: luchta-resident-workers
---

> [!NOTE]
> The worker protocol terminal `Done` message no longer carries inputs. Inputs are now reported during the resolve phase. See [worker-inputs-moved-to-resolve-phase-2026-07-05.md](../logic-errors/worker-inputs-moved-to-resolve-phase-2026-07-05.md).

## Problem

Stay-resident worker processes for the luchta task runner introduced multiple concurrency bugs: (1) shutdown timeout starvation when a reaper task held a child-wait lock across `child.wait().await`, (2) TOCTOU duplicate-spawn race in `get_or_spawn` allowing two concurrent callers to both spawn workers, (3) worker protocol liveness violation where workers consuming requests but not emitting terminal `Done` hung the engine's `run_job` forever, and (4) ungated Unix-only APIs breaking Windows builds in a cross-platform workspace.

## Symptoms

- **Shutdown timeout starvation**: A worker that ignores stdin-EOF (e.g., `sleep 60`) hung the run for the full process lifetime (60s) instead of the 5s shutdown timeout. Reaper held exclusive wait lock, blocking shutdown path from reaching timeout→kill.
- **Duplicate worker spawn**: Under concurrent first-use of the same worker name, two processes spawned; one leaked (lingering orphan).
- **Engine hang on worker failure**: Worker that spawns a job which fails immediately (e.g., invalid cwd) returned error without emitting `Done` → engine's `run_job` loop waited on `rx.recv()` forever (no EOF, no crash).
- **Windows build break**: Release workflow targets Windows MSVC, but library crate used ungated `std::os::unix`, `libc::kill`, `ExitStatusExt`. CI only ran ubuntu, so break wasn't caught until cross-compilation.

```text
Error: cannot find value `process_group` in this scope
Error: `ExitStatusExt` is not available on Windows
```

## Investigation Steps

1. **Shutdown hang**: Enabled debug logging in `WorkerHandle::shutdown`. Found reaper task held `child` mutex across `child.wait().await`. Shutdown path tried to lock child → blocked → timeout never reached.

2. **Duplicate spawn race**: Reviewed `get_or_spawn` (manager.rs ~173). Pattern: lock workers → cache miss → release lock → spawn_worker().await → re-lock → `or_insert_with(handle)`. Two concurrent callers both miss cache, both spawn, one wins insertion, other handle orphans.

3. **Worker hang on failure**: Traced `luchta-yarn-worker` `handle_request`: `run_one_job(...).await?` early-returns on spawn failure BEFORE emitting `Done`. Worker process stays alive → engine waits indefinitely.

4. **Windows break**: Inspected `.github/workflows/release.yaml` — builds for 3 Windows MSVC targets. Ran `cargo check --target x86_64-pc-windows-msvc` locally (or reviewed imports) — found ungated Unix imports at file scope.

## Root Cause

1. **Lock starvation**: Reaper task owned `child.wait().await` while holding exclusive lock. Shutdown needed same lock to implement timeout-then-kill. Result: bounded shutdown (5s) defeated by unbounded wait.

2. **TOCTOU race**: Double-checked locking pattern incomplete. Cache check released lock before spawn, allowing interleaving.

3. **Protocol violation**: `Done` response not guaranteed on all error paths. JSONL protocol requires terminal response per request for engine to resolve job.

4. **Ungated Unix imports**: `std::os::unix::process::ExitStatusExt`, `libc::kill`, `CommandExt::process_group` used at file scope without `#[cfg(unix)]`. Library compiles for all targets, but Unix APIs don't exist on Windows.

## Solution

### 1. Reaper/Shutdown Lock-Starvation Fix

Reaper owns `child.wait()` exclusively; shutdown uses separate exit signal:

```rust
// handle.rs
pub(crate) struct WorkerHandle {
    pub(crate) child: Arc<Mutex<Option<Child>>>,
    pub(crate) exit_notify: Arc<Notify>,  // NEW: signaled when reaper completes
    pub(crate) pgid: i32,
    // ...
}

impl WorkerHandle {
    pub(crate) async fn shutdown(&self, shutdown_timeout: Duration) {
        if self.is_shutdown.swap(true, Ordering::SeqCst) {
            return;  // Already shutting down
        }
        
        // Drop writer (closes stdin)
        self.writer_tx.lock().await.take();
        
        // Wait for exit signal with timeout — NO child lock needed
        if timeout(shutdown_timeout, self.exit_notify.notified()).await.is_err() {
            // Timeout: kill process group
            kill_process_group(self.pgid);
            wait_for_reaper_completion(&self.reaper_task).await;
        } else {
            wait_for_reaper_completion(&self.reaper_task).await;
        }
        
        self.abort_tasks().await;
        self.child.lock().await.take();  // Now safe to clear
    }
}

// io_tasks.rs — reaper signals exit WITHOUT holding child lock during wait
async fn reap_child(child: Arc<Mutex<Option<Child>>>, exit_notify: Arc<Notify>) {
    let mut child_guard = child.lock().await;
    let Some(process) = child_guard.as_mut() else { return };
    let _ = process.wait().await;  // Holds process lock, not child mutex
    child_guard.take();
    drop(child_guard);  // Release before notify
    exit_notify.notify_one();
}
```

**Why this works**: Shutdown waits on `Arc<Notify>`, not on the child lock. Reaper signals completion after `wait()`. Kill happens without lock contention.

### 2. TOCTOU Duplicate-Spawn Race Fix

Dedicated spawn mutex ensures atomicity:

```rust
pub(crate) struct WorkerManager {
    workers: Mutex<HashMap<String, WorkerHandle>>,
    spawn_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,  // NEW: per-worker spawn mutex
    // ...
}

impl WorkerManager {
    async fn get_or_spawn(&self, name: &str) -> Result<Arc<WorkerHandle>, WorkerError> {
        // Fast path: already spawned
        if let Some(handle) = self.workers.lock().await.get(name) {
            return Ok(Arc::clone(handle));
        }
        
        // Get or create per-worker spawn lock
        let spawn_lock = {
            let mut locks = self.spawn_locks.lock().await;
            Arc::clone(locks.entry(name.to_owned()).or_insert_with(|| Arc::new(Mutex::new(()))))
        };
        
        // Hold spawn lock for entire check-spawn-insert sequence
        let _guard = spawn_lock.lock().await;
        
        // Double-check: another caller may have spawned while we waited
        if let Some(handle) = self.workers.lock().await.get(name) {
            return Ok(Arc::clone(handle));
        }
        
        // We hold spawn lock, workers lock released — safe to spawn
        let handle = self.spawn_worker(name).await?;
        
        // Insert while spawn lock still held
        self.workers.lock().await.insert(name.to_owned(), Arc::clone(&handle));
        
        Ok(handle)
    }
}
```

**Why this works**: Per-worker spawn mutex serializes first-time spawns. Double-check inside spawn lock prevents duplicate insertion. Loser waits, re-checks, finds existing handle.

### 3. Worker-Protocol Liveness Guarantee

Worker MUST emit terminal `Done` on all paths:

```rust
// luchta-yarn-worker/src/main.rs
async fn handle_request(req: WorkerRequest, stdout: Arc<Mutex<Stdout>>) -> i32 {
    let result = run_one_job(&req).await;
    let exit_code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("worker error: {}", e);
            1  // Non-zero exit on failure
        }
    };
    
    // ALWAYS emit Done, even on error
    let response = WorkerResponse::done(&req.id, exit_code);
    write_response(&stdout, &response).await;
    
    exit_code
}
```

**Why this works**: Engine's `run_job` waits for `Done` or EOF/crash. If worker stays alive but never sends `Done`, job hangs. Guaranteed emission on all paths prevents indefinite hang.

### 4. Unix-Only Feature Gating

Gate Unix-only code; provide stubbed public API for non-Unix:

```rust
// worker/mod.rs
pub mod manager;
pub mod protocol;

#[cfg(unix)]
mod handle;
#[cfg(unix)]
mod io_tasks;
#[cfg(unix)]
mod spawn;

// worker/manager.rs
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use crate::worker::protocol::WorkerResponse;

#[cfg(unix)]
pub struct WorkerManager { /* full impl */ }

#[cfg(unix)]
impl WorkerManager {
    pub async fn run_job(&self, worker_name: &str, request: WorkerRequest) -> Result<i32, WorkerError> {
        // Real impl...
    }
}

// Non-Unix stub with SAME public API
#[cfg(not(unix))]
#[derive(Debug)]
pub struct WorkerManager {
    definitions: HashMap<String, WorkerDefinition>,
    shutdown_timeout: Duration,
    prefix_width: usize,
}

#[cfg(not(unix))]
impl WorkerManager {
    pub fn new(definitions: HashMap<String, WorkerDefinition>) -> Self { /* ... */ }
    pub fn with_shutdown_timeout(definitions: HashMap<String, WorkerDefinition>, timeout: Duration) -> Self { /* ... */ }
    pub fn with_prefix_width(self, width: usize) -> Self { /* ... */ }
    
    pub async fn run_job(&self, worker_name: &str, request: WorkerRequest) -> Result<i32, WorkerError> {
        // Return clear error on non-Unix
        Err(WorkerError::Unsupported {
            worker: worker_name.to_owned(),
            id: request.id,
        })
    }
    
    pub async fn shutdown(&self) {}
}

#[cfg(all(test, unix))]  // Gate test module too
mod tests;
```

**Gate imports used only by Unix impl** (else unused-import fails `-D warnings` on non-Unix):

```rust
#[cfg(unix)]
use tokio::sync::Mutex;
```

### 5. ExitStatus Synthesis (Unix)

`ExitStatus::from_raw(code)` misreads bare exit code as signal. Correct synthesis:

```rust
#[cfg(unix)]
fn synthesize_exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    // Shift exit code left by 8: status byte is high bits, signal is low bits
    ExitStatusExt::from_raw((code & 0xff) << 8)
}
```

**Why this works**: Unix `wait` status encodes exit code in bits 8-15, signal in bits 0-7. Shift ensures `.code()` and `.success()` return correct values.

### 6. Tokio JSONL IPC Pattern

Avoid pipe-buffer deadlock with separate read/write tasks:

```rust
// io_tasks.rs
const MAX_LINE_LENGTH: usize = 1 << 20;  // 1 MiB bound

pub(crate) fn spawn_reader_task(
    stdout: ChildStdout,
    jobs: JobMap,
    is_shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let framed = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_LENGTH));
        let mut lines = tokio_stream::wrappers::LinesCodecStream::new(framed);
        
        while let Some(line_result) = lines.next().await {
            match line_result {
                Ok(line) => { /* route by job id */ }
                Err(LinesCodecError::MaxLineLengthExceeded) => {
                    crash_all_jobs(&jobs).await;
                    return;
                }
                Err(_) => return,
            }
        }
    })
}

// NEVER in same task:
// while let Some(line) = lines.next().await { ... }
// stdin.write_all(...).await; // DEADLOCK: pipe buffers full
```

**Actor pattern for response routing**:

```rust
pub(crate) type JobMap = Arc<Mutex<HashMap<String, mpsc::Sender<WorkerResponse>>>>;

// On request, create channel and register:
let (tx, mut rx) = mpsc::channel(1);
jobs.lock().await.insert(request.id.clone(), tx);
// ... send request

// On response, route by id:
if let Some(sender) = jobs.lock().await.get(&response.id()) {
    sender.send(response).await.ok();
}
```

## Why This Works

1. **Reaper/shutdown decoupling**: `Arc<Notify` lets shutdown await reaper completion without contending for child lock. Timeout fires even if worker ignores stdin-EOF.

2. **Per-worker spawn mutex**: Only one caller acquires spawn lock per worker name. Double-check inside lock prevents race without serializing all spawns globally.

3. **Protocol liveness invariant**: Engine can rely on `Done` or EOF/crash to resolve every job. No indefinite hangs from worker-local errors.

4. **Cross-platform compilation**: Public API consistent across cfg variants. Library compiles on Windows; non-Unix workers fail fast with clear `Unsupported` error.

5. **Bounded memory**: `LinesCodec::new_with_max_length(1 MiB)` caps per-line buffer. Oversized lines crash jobs cleanly rather than consuming unbounded memory.

6. **ExitStatus correctness**: Bit shift aligns with kernel `wait` status encoding, so `ExitStatus::code()` and `success()` behave identically to normal process exit.

## Prevention Strategies

### Test Cases

- **Shutdown timeout**: Worker script with trailing `sleep 60` after EOF. Assert shutdown completes within 2x configured timeout.
- **Duplicate spawn stress**: N concurrent `run_job` calls for same new worker. Assert exactly ONE process spawned (PID marker file with single line).
- **Worker failure**: Request with invalid cwd. Assert `Done` still emitted with non-zero exit code.
- **Cross-platform check**: `cargo check --target x86_64-pc-windows-msvc` (or review imports if target unavailable).
- **Oversized line**: Worker emits >1 MiB JSON line. Assert engine returns `Crashed` error.

### Best Practices

- Never hold lock across `child.wait().await` if another path needs timeout-then-kill
- Use `Arc<Notify>` for exit signaling; avoid lock-based wait coordination
- Always use double-checked locking with dedicated spawn mutex for lazy init under concurrency
- Gate Unix-only code with `#[cfg(unix)]`; provide same public API on non-Unix
- Gate imports used only by Unix impl to avoid unused-import warnings
- Gate test modules `#[cfg(all(test, unix))]`
- Split tokio process read/write into separate tasks
- Use `FramedRead` + `LinesCodec` with max-length for bounded framing

### Code Review Checklist

- [ ] Shutdown path doesn't require lock held across `child.wait()`?
- [ ] Lazy spawn uses per-resource mutex, not just cache-then-spawn?
- [ ] Worker protocol guarantees terminal response on all error paths?
- [ ] Unix-only APIs gated with `#[cfg(unix)]`?
- [ ] Unix-only imports gated (not just usage)?
- [ ] Test modules gated `#[cfg(all(test, unix))]` if they use `/proc`, `PermissionsExt`, `libc`?
- [ ] No read+write in same tokio task for process IPC?
- [ ] LinesCodec configured with max-length bound?

## Related Issues

- **Prior Art:** [executable-config-loader-hardening-2026-06-08.md](./executable-config-loader-hardening-2026-06-08.md) — Process-group kill, ETXTBSY retry, timeout-then-kill pattern (foundational for worker shutdown)
- **GitHub:** [dobesv/luchta#1](https://github.com/dobesv/luchta/issues/1) — Stay-resident worker processes feature
- **Plan:** `luchta-resident-workers` — Full implementation history in plan notes
