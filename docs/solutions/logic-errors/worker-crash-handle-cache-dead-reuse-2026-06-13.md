---
title: "Worker crash handle-cache dead-reuse race and missing crash diagnostics"
date: 2026-06-13
category: logic-errors
problem_type: logic_error
component: luchta-engine/worker
root_cause: "Cached dead worker handle reused after crash; crash error lacked diagnostics"
resolution_type: code_fix
severity: high
tags:
  - concurrency
  - race-condition
  - process-management
  - caching
  - dead-handle
  - liveness-check
  - diagnostics
  - stderr-capture
plan_ref: luchta-worker-crash-hang
---

## Problem

WorkerManager's handle cache allowed dead worker handles to be reused after a crash, causing tasks to hang forever. Additionally, `WorkerError::Crashed` carried no exit status or stderr — diagnostics were lost because worker stderr was `Stdio::inherit()`.

## Symptoms

- **Permanent hang**: After a worker crashed at OS level, subsequent tasks dispatched to that worker hung in "running" state forever; the run never drained.
- **No crash diagnostics**: When `WorkerError::Crashed` surfaced, it had only `worker, id` — no exit code, no stderr. Root cause (`io error: Resource temporarily unavailable (os error 11)` = EAGAIN) was invisible to the user.
- **Silent failure**: Worker stderr printed to terminal but couldn't be tied to the failing task.

## Investigation Steps

1. Reproduced hang: killed worker process externally, then dispatched new job — hung indefinitely.
2. Traced dispatch path: `get_or_spawn` returned cached `WorkerHandle` even after process exited. Handle's `jobs` map was empty, `writer_tx` was still present.
3. Found dispatch race:
   - Crash path cleared per-job map (`jobs.clear()`) but never evicted dead handle from `self.workers` and never cleared `writer_tx`.
   - Post-crash dispatch got dead handle, registered in abandoned job map, `writer_tx.send` succeeded into dead pipe, `rx.recv()` awaited response that could never arrive.
4. Found diagnostics gap: `WorkerError::Crashed { worker, id }` had no exit status/stderr. Worker stderr was `Stdio::inherit()` — printed to terminal untied to any task.

## Root Cause

**Dead-handle reuse race**: The handle cache (`HashMap<WorkerName, WorkerHandle>`) was never validated for liveness. After crash:
1. Crash path cleared `jobs` map (in-flight work) but left dead handle in cache.
2. `writer_tx` (mpsc sender to worker stdin) was never cleared on crash.
3. New dispatch reused dead handle, inserted into abandoned `jobs` map.
4. `writer_tx.send` succeeded into pipe with no reader; `rx.recv()` blocked forever.
5. Walker never drained — whole run hung.

**Missing crash diagnostics**: `Stdio::inherit()` lets subprocess stderr escape orchestrator. No exit status or stderr captured means `WorkerError::Crashed` cannot surface root cause.

## Solution

### 1. Liveness-Check on Handle Reuse

`get_or_spawn` → `try_reuse_worker` checks `is_alive()` before returning cached handle:

```rust
// handle.rs
impl WorkerHandle {
    pub(crate) fn is_alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst) && self.writer_tx.lock().unwrap().is_some()
    }
}

// manager.rs
fn try_reuse_worker(&mut self, name: &str) -> Option<WorkerHandle> {
    let handle = self.workers.get(name)?;
    if !handle.is_alive() {
        self.workers.remove(name);  // Evict dead handle
        return None;
    }
    Some(handle.clone())
}
```

Reaper task evicts handle and clears `writer_tx` on process exit.

### 2. Defense-in-Depth Against Hang

Three independent paths resolve job rather than block:
1. **Channel-close detection**: If `writer_tx` is None, dispatch fails fast.
2. **Send-failure on dead pipe**: If send fails, job resolves with error.
3. **Reaper eviction**: On exit, reaper clears `writer_tx` and evicts handle.

Net: post-crash dispatch either respawns healthy worker or fails fast — can never hang.

### 3. Capture Crash Diagnostics

Switch stderr from `inherit` to `piped` + drain task into bounded ring buffer:

```rust
// io_tasks.rs
let stderr = child.stderr.take()?;
let stderr_tail = Arc::new(Mutex::new(RingBuffer::new(1024)));  // 1KB tail

spawn(stderr_drain_task(stderr, stderr_tail.clone()));

// handle.rs — attach to crash error
pub(crate) struct WorkerCrashInfo {
    pub(crate) exit_status: Option<ExitStatus>,
    pub(crate) stderr_tail: String,
}

impl WorkerError {
    pub(crate) fn crashed_error(worker: &str, id: &str, info: WorkerCrashInfo) -> Self {
        let detail = info.exit_status.as_ref().map(|s| format!(" (exit: {})", s));
        let detail_suffix = if !info.stderr_tail.is_empty() {
            format!(": {}{}", detail.as_deref().unwrap_or(""), info.stderr_tail)
        } else { detail.unwrap_or_default() };
        WorkerError::Crashed { worker: worker.into(), id: id.into(), detail, detail_suffix }
    }
}
```

CLI `format_task_error` routes full `ExecutorError` (including crash detail) to terminal. Interrupt suppression preserved.

## Why This Works

1. **Liveness check prevents dead-handle reuse**: `is_alive()` checks both `exited` flag (reaper sets on exit) and `writer_tx` presence. Dead handle evicted before dispatch can use it.

2. **Defense-in-depth**: Even if TOCTOU race (handle dies after check but before send), send-failure or reaper eviction resolves job — no single point of failure.

3. **Bounded stderr capture**: Ring buffer caps memory (1KB). Drain task ensures stderr doesn't block on full buffer. Tail attached to error surfaces real root cause.

4. **No silent diagnostics**: Crash error now carries exit status + stderr tail. User can diagnose EAGAIN, OOM, segfault, etc.

## Prevention Strategies

### Test Cases

Added regression tests (manager/tests.rs):
- **post_crash_job_returns_within_timeout**: Kill worker externally, dispatch job — assert returns `Crashed` within timeout (no hang).
- **crashed_worker_is_evicted_and_respawned**: Kill worker, dispatch job — assert new process spawned, job succeeds.
- **crash_error_includes_exit_status_and_stderr_detail**: Kill worker with signal, dispatch job — assert error carries exit status + stderr.

All tests validated under `nextest --stress-count=5`.

### Best Practices

- **Liveness-check cached process handles**: Never reuse cached handle without checking process is still alive.
- **Evict on death path**: Reaper must evict handle from registry, not just clear in-flight work.
- **Clear sender on death**: `writer_tx` must be cleared when process exits, otherwise send into dead pipe succeeds but response never arrives.
- **Pipe + capture bounded stderr tail**: Never `inherit` stderr for long-lived workers — diagnostics escape orchestrator. Pipe + bounded ring buffer preserves root cause.
- **Defense-in-depth for hangs**: Channel-close + send-failure + reaper eviction — three independent paths to resolve job.

### Code Review Checklist

- [ ] Cached process handles validated for liveness before reuse?
- [ ] Crash/exit path evicts handle from cache?
- [ ] `writer_tx` cleared on process death?
- [ ] Worker stderr piped + captured (not inherited)?
- [ ] Crash error carries exit status + stderr tail?
- [ ] Hang regression tests for post-crash dispatch?

## Related Issues

- **Prior Solution:** [resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — JSONL protocol liveness, shutdown/reaper patterns
- **Plan:** `luchta-worker-crash-hang` — Full implementation history in plan notes
