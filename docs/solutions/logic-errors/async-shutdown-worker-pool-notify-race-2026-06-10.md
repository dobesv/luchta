---
title: "Graceful async shutdown of tokio worker pools and tokio Notify missed-wakeup race"
date: 2026-06-10
category: logic-errors
problem_type: logic_error
component: luchta-engine/worker
root_cause: "Incomplete cleanup of in-flight jobs during shutdown and TOCTOU race in check-then-wait pattern with tokio Notify"
resolution_type: code_fix
severity: high
tags:
  - tokio
  - async-shutdown
  - worker-pool
  - race-condition
  - signal-handling
  - Notify
  - graceful-termination
plan_ref: luchta-deps-and-shutdown
---

## Problem

SIGINT to `luchta run` hung indefinitely. Root cause: worker shutdown never cleared in-flight jobs, so blocked `run_job` tasks awaiting channel responses never resolved. Additionally, a check-then-wait pattern with `tokio::sync::Notify` had a TOCTOU race causing missed wakeups.

## Symptoms

- `luchta run` hangs on Ctrl+C, never returns to prompt
- After interrupt, worker subprocesses and child `yarn` processes remain alive (orphans)
- Broken-pipe/job-failed error spam on terminal after parent exits
- `luchta run ... | head` panics with "failed printing to stdout: Broken pipe"
- Task ordering bugs in generated config appeared as tool bugs (misdiagnosis)

```text
# Hang: walker.wait() never completes
^C
<hangs forever>

# Noise: per-task crash output on interrupt (N concurrent tasks = N lines)
Worker { name: "yarn-worker" .. Crashed .. }
Worker { name: "yarn-worker" .. Crashed .. }
...

# SIGPIPE panic when piped
thread 'main' panicked at 'failed printing to stdout: Broken pipe'
```

## Investigation Steps

1. **Hang diagnosis**: Signal handler fired correctly. Added debug logging — `worker_manager.shutdown()` returned but `walker.wait()` blocked forever. Traced to `run_job` blocked on `rx.recv()` waiting for response from worker; sender (`tx`) stored in `handle.jobs` map.

2. **Root cause of hang**: `WorkerHandle::shutdown` never called `crash_all_jobs` to clear the jobs map. In-flight `run_job` calls blocked forever because their response senders stayed alive.

3. **Spawn-during-shutdown race**: Under concurrency limit, queued tasks could spawn new workers right as shutdown started. Traced the window: `is_shutdown` check → deschedule → flag set → worker spawned after shutdown began.

4. **Notify race discovery**: `wait_for_exit_signal` did `if exited.load() { return } else { timeout(notify.notified()).await }`. If notifier set flag and called `notify_waiters()` between flag check and registering waiter, wakeup lost → timeout.

5. **Config bug vs tool bug**: User reported "dependency not honored". Adding `--dry-run` revealed the generated config had overwritten `dependsOn` via later JS object spread — last key wins. Not a tool bug.

## Root Cause

### 1. Incomplete job cleanup during shutdown

`run_job` spawns a tokio task that sends request via `tx`, then awaits `rx.recv()`. Sender `tx` stored in `handle.jobs` map. Shutdown must clear this map to drop senders, which closes channels and unblocks receivers.

Without clearing jobs:
- Shutdown kills worker process
- Worker never sends response
- `rx.recv()` waits forever (channel still open — sender exists in map)
- Walker never receives completion signal
- `walker.wait()` hangs

### 2. Notify check-then-wait TOCTOU

Pattern:
```rust
if exited.load(Ordering::SeqCst) {
    return;
}
// WINDOW: notifier sets flag, calls notify_waiters()
timeout(Duration::from_secs(5), notify.notified()).await;
// If notify_waiters() called in window, waiter not yet registered → lost
```

Between the flag check and `notified()` future registration, another task can signal completion. The waiter isn't registered yet, so `notify_waiters()` doesn't wake it.

### 3. Worker spawn race

`is_shutdown` flag check raced with worker spawn. Task passes check → yields → shutdown sets flag → task resumes and spawns worker. The `shutdown_all` loop missed workers spawned right before flag was set.

### 4. SIGPIPE panic

Rust ignores SIGPIPE by default. `println!` writes to stdout, pipe closes, EPIPE returned, Rust panics instead of silently exiting.

## Solution

### 1. Clear in-flight jobs during shutdown

```rust
// handle.rs - WorkerHandle::shutdown
pub(crate) async fn shutdown(&self, shutdown_timeout: Duration) {
    if self.is_shutdown.swap(true, Ordering::SeqCst) {
        return; // Already shutting down
    }
    
    // Drop writer (closes stdin, signals worker to drain)
    self.writer_tx.lock().await.take();
    
    // Wait for reaper with timeout
    if timeout(shutdown_timeout, self.exit_notify.notified()).await.is_err() {
        self.kill_process_group(); // SIGKILL fallback
    }
    
    // CRITICAL: clear jobs so blocked rx.recv() unblocks
    crash_all_jobs(&self.jobs);
}
```

### 2. Canonical fix for Notify missed-wakeup

```rust
// WRONG: TOCTOU race
if exited.load(Ordering::SeqCst) {
    return;
}
timeout(duration, notify.notified()).await; // can miss signal

// CORRECT: register waiter BEFORE checking flag
let notified = notify.notified();
pin!(notified);
if notified.as_mut().enable() || exited.load(Ordering::SeqCst) {
    return; // Already notified OR flag set
}
timeout(duration, notified).await;
```

`Notified::enable()` registers the waiter immediately and returns `true` if already notified. This eliminates the window.

### 3. Shutdown ordering and straggler loop

```rust
// run.rs - interrupt path
async fn finalize_run(interrupted: bool, worker_manager: &WorkerManager, walker: &mut Walker) {
    if interrupted {
        drop(receiver); // Walker can't send to closed channel
        worker_manager.shutdown_immediate().await; // Kill workers first
    }
    
    walker.wait().await; // Now drains without hanging
}

// manager.rs - catch stragglers
pub async fn shutdown_all(&self, timeout: Duration) {
    self.is_shutdown.store(true, Ordering::SeqCst);
    
    // Loop catches workers spawned right before flag was set
    loop {
        let handles: Vec<_> = self.workers.lock().await.drain().collect();
        if handles.is_empty() {
            break;
        }
        for (_, handle) in handles {
            handle.shutdown(timeout).await;
        }
    }
}

// run_job guard - prevent post-shutdown spawns
pub async fn run_job(&self, name: &str, req: WorkerRequest) -> Result<i32, WorkerError> {
    if self.is_shutdown.load(Ordering::SeqCst) {
        return Err(WorkerError::Crashed { ... });
    }
    // ... spawn
}
```

**Ordering matters**: On interrupt, kill workers BEFORE `walker.wait()` (workers' death resolves in-flight jobs, lets walker drain). On normal completion, walker already drained so shutdown happens after.

### 4. Suppress noise and graceful kill escalation

```rust
// run.rs - shared interrupted flag
let interrupted = Arc::new(AtomicBool::new(false));

// In task runner, suppress crash output if interrupted
if !interrupted.load(Ordering::SeqCst) {
    report_task_failure(&task, &err);
}

// io_tasks.rs - graceful escalation
const TERMINATE_GRACE: Duration = Duration::from_secs(1);

async fn terminate_gracefully(pgid: i32) {
    terminate_process_group(pgid); // SIGTERM first
    tokio::time::sleep(TERMINATE_GRACE).await;
    kill_process_group(pgid); // SIGKILL only if still alive
}
```

### 5. SIGPIPE reset

```rust
// main.rs - before any output
#[cfg(unix)]
fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
```

### 6. Dry-run for config diagnosis

```rust
// run.rs - longest-path layering
fn compute_execution_waves(tasks: &[Task], graph: &TaskGraph) -> Vec<Vec<TaskId>> {
    let mut depths = HashMap::new();
    
    fn resolve_depth(id: TaskId, graph: &TaskGraph, depths: &mut HashMap<TaskId, u32>) -> u32 {
        if let Some(&d) = depths.get(&id) {
            return d;
        }
        let max_dep = graph.dependencies(id)
            .iter()
            .filter(|dep| tasks.contains(dep))
            .map(|dep| resolve_depth(*dep, graph, depths) + 1)
            .max()
            .unwrap_or(0);
        depths.insert(id, max_dep);
        max_dep
    }
    
    for task in tasks {
        resolve_depth(*task, graph, &mut depths);
    }
    
    // Group by depth => parallel "waves"
    let mut waves: HashMap<u32, Vec<TaskId>> = HashMap::new();
    for (id, depth) in depths {
        waves.entry(depth).or_default().push(id);
    }
    // ... sort and return
}
```

Usage: `luchta run --dry-run` prints execution waves without running, revealing config errors like dropped dependencies.

### 7. chmod on read-only FS

```rust
// config.rs - skip chmod if already executable
if (metadata.mode() & 0o100) == 0 {
    // Owner-execute bit not set, try to chmod
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    // Ignore EROFS - can't set bits on read-only mount
}
```

## Why This Works

1. **Job cleanup**: Dropping senders from `jobs` map closes channels. `rx.recv()` returns `None` (channel closed), unblocking all in-flight `run_job` calls.

2. **Notify pattern**: `Notified::enable()` registers waiter atomically before flag check. If already notified, returns `true` immediately. No window for missed wakeup.

3. **Straggler loop**: Workers that passed `is_shutdown` check before flag was set get caught in subsequent drain iteration. Loop terminates when no workers remain.

4. **Graceful escalation**: SIGTERM lets node/babel/yarn clean up (no stack traces). SIGKILL only if process ignores SIGTERM for 1 second.

5. **SIGPIPE DFL**: Restores default Unix behavior — process exits silently on broken pipe instead of panicking.

6. **Dry-run**: Longest-path layering matches walker semantics exactly. Config bugs visible without executing.

## Prevention Strategies

### Test Cases

- Interrupt stress: SIGINT with N concurrent tasks, assert prompt exit, no orphans, no noise (single `× interrupted` line)
- Notify race: Two tasks, one signals immediately after flag set, other waits — assert no timeout
- Shutdown spawn race: Concurrent job dispatch + interrupt, assert no orphan workers
- Pipeline: `luchta run ... | head`, assert quiet exit (no panic)
- Dry-run validation: Config with overwritten `dependsOn`, assert wave output shows missing dependency

### Best Practices

- Always clear channel senders before awaiting walker drain in shutdown
- Use `Notified::enable()` pattern for check-then-wait on `tokio::sync::Notify`
- Loop until empty in shutdown when spawn race exists
- Suppress per-task crash output once interrupted flag set
- SIGTERM before SIGKILL for graceful child termination
- Reset SIGPIPE to `SIG_DFL` early in main on Unix
- Provide dry-run/plan view for generated config debugging
- Skip idempotent chmod operations on read-only filesystems

### Code Review Checklist

- [ ] Shutdown clears all channel senders that receivers await?
- [ ] Notify waiters registered before flag check?
- [ ] Shutdown loop handles spawn-during-shutdown race?
- [ ] Signal handlers terminate workers before awaiting walker?
- [ ] Interrupted flag suppresses per-task failure output?
- [ ] Grace escalation (SIGTERM → wait → SIGKILL) for process groups?
- [ ] SIGPIPE reset at startup on Unix?
- [ ] Dry-run feature available for diagnosing generated config?

## Related Issues

- **Related:** [integration-issues/resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — Foundational worker lifecycle, `Arc<Notify>` for exit signaling, process-group management
- **Plan:** `luchta-deps-and-shutdown` — Full implementation history with e2e verification
