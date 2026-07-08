---
title: "Worker crash retry-once at dispatch layer (issue #171)"
date: 2026-07-07
category: logic-errors
problem_type: logic_error
component: luchta-engine/worker
root_cause: "Worker crash mid-task caused immediate task failure without retry"
resolution_type: code_fix
severity: medium
tags:
  - retry
  - crash-handling
  - process-management
  - dispatch-layer
  - testing-technique
plan_ref: luchta-worker-crash-retry
---

## Problem

Resident workers occasionally crash mid-task (OOM, segfault, process exit). Engine marked task failed immediately, even though crash is transient and a fresh worker could complete the task.

## Symptoms

- Task failure with `WorkerError::Crashed` on worker crash, even for idempotent operations
- No retry mechanism for transient worker failures
- User-visible task failure for what should be recoverable
- No diagnostic indicating retry occurred

## Solution

Added retry-exactly-once at the DISPATCH layer in `WorkerManager`, not inside the shared `round_trip()` IPC primitive.

### Implementation

New private helper `round_trip_retry_once_on_crash<T,F>` centralizes retry logic for both run path and resolve path:

```rust
async fn round_trip_retry_once_on_crash<T, F>(
    &self,
    worker_name: &str,
    first_message: WorkerMessage,
    retry_message: WorkerMessage,
    sink: Option<&ExecutionLogSink>,
    select: F,
) -> Result<T, WorkerError>
where
    F: Fn(WorkerResponse) -> Option<T> + Copy,
{
    match self.round_trip(worker_name, first_message, |_, _, _| {}, sink, select).await {
        Err(WorkerError::Crashed { worker, id, .. }) => {
            eprintln!("warning: worker '{worker}' crashed during job '{id}', retrying once",);
            self.round_trip(worker_name, retry_message, |_, _, _| {}, sink, select).await
        }
        other => other,
    }
}
```

**Key decisions:**
- Retry ONLY on `WorkerError::Crashed` — pass `Protocol`, `Spawn`, `Undefined`, `Unsupported` and normal task failures through unchanged
- Max 2 attempts, no backoff (crash handling already waits 250ms), no config/opt-out
- Request payload cloned before first attempt (`WorkerRequest` + `ResolveTask` derive `Clone`)
- `ExecutionLogSink` reused across attempts — both attempts' logs kept as diagnostic trail
- `resolve()` detects `Crashed` and retries BEFORE mapping `WorkerError→String`, preserving its `Result<_, String>` signature
- Respawn is automatic: crashed worker already evicted during crash handling; `get_or_spawn` respawns lazily on next dispatch

## Why This Works

Worker crash handling already evicts dead worker from `self.workers` cache. On retry, `get_or_spawn()` spawns fresh process. No explicit respawn code needed — existing lazy-spawn mechanism handles it.

Retry at dispatch layer preserves task context (job ID, request payload). `round_trip()` remains a simple IPC primitive without retry logic, avoiding blind retry loops.

## Non-Obvious Learnings

### 1. `WorkerError::Spawn` is UNREACHABLE through shell-based test harness

Workers launch via `sh -c <command_line>` (in `spawn_worker_process`). Missing/non-executable worker binary makes `sh` exit 127/126, which surfaces as `WorkerError::Crashed`, NOT `Spawn`.

**Consequence:** Integration test for spawn-no-retry was removed — the branch is unreachable via shell. `Protocol`-no-retry test covers non-retried-error branch instead.

### 2. Test technique for exact attempt counts

Shell mock worker increments per-request counter file at TOP of script on every process spawn:

```sh
#!/bin/sh
count_file="/path/to/spawn-count.txt"
count=0
if [ -f "$count_file" ]; then
  count=$(cat "$count_file")
fi
count=$((count + 1))
echo "$count" > "$count_file"
# ... rest of worker
```

Counter value equals number of dispatch attempts. Tests assert `spawn_count == N` after single `run_job`/`resolve` call.

### 3. Adding transparent retry changes existing crash test expectations

Two pre-existing crash tests asserted `expect_err` on single crash that is now transparently retried. Correct fix: REWRITE to new behavior while preserving unique guards (distinct-PID respawn assertion, `tokio::time::timeout` hang-guard), NOT `#[ignore]`.

### 4. Do NOT retry a "crash" that is really shutdown killing the worker

**Follow-up bug (found in production):** On a real task failure the run tears down via
`WorkerManager::shutdown()` → `shutdown_all()`, which kills every resident worker that still
has an in-flight job. Each killed job's response channel closes (stdout EOF), which
`round_trip()` classifies as `WorkerError::Crashed`. The retry helper then fired a spurious
`warning: worker '...' crashed ... retrying once` **and** a pointless retry for *every*
in-flight job during teardown. One genuine failure produced ~15 bogus warnings.

**Fix:** guard the retry on the existing `is_shutdown: Arc<AtomicBool>` flag. It is set true
at the START of `shutdown_all` (`swap(true, SeqCst)`) and in `Drop`, BEFORE any worker is
killed — so the flag is always observed true before the channel-close it causes. Only warn +
retry when `!self.is_shutdown.load(Ordering::SeqCst)`; during shutdown, return the original
`Crashed` error unchanged:

```rust
Err(WorkerError::Crashed { worker, id, .. })
    if !self.is_shutdown.load(Ordering::SeqCst) =>
{
    eprintln!("warning: worker '{worker}' crashed during job '{id}', retrying once");
    self.round_trip(worker_name, retry_message, |_, _, _| {}, sink, select).await
}
other => other,
```

**Regression-test gotcha:** asserting `spawn_count == 1` is NOT enough — during shutdown
`dispatch_message` refuses to spawn a worker either way, so the count stays 1 even with the
bug. The discriminator is the **error detail**: the guarded (correct) path returns the killed
worker's crash detail (signal/exit status) gathered on the first attempt, while the unguarded
(buggy) retry returns a *detail-less* `Crashed` from `dispatch_message`'s shutdown refusal.
Assert the error carries detail (e.g. `assert_crash_detail_contains(&error, &[..., "signal"])`);
verified by mutation (`guard → if true` fails the detail assertion).

## Prevention Strategies

**Test cases that must pass:**
1. Crash on attempt 1, succeed on attempt 2 → `Ok`, spawn_count == 2, distinct PIDs
2. Crash on both attempts → `WorkerError::Crashed`, spawn_count == 2 (no third)
3. Non-zero task exit (normal failure) → NOT retried, spawn_count == 1
4. `WorkerError::Protocol` → NOT retried, spawn_count == 1
5. Crash observed while shutting down → NOT retried, NOT warned; original `Crashed` error
   (with detail) returned unchanged

**Code review checklist:**
- [ ] Retry only on `Crashed`, not on other `WorkerError` variants
- [ ] Warning emitted before retry via `eprintln!`
- [ ] Request cloned before first attempt
- [ ] No new log/tracing dependency (`luchta-engine` uses `eprintln!`)
- [ ] Both `run_job` and `resolve` paths tested

## Related Issues

- **GitHub:** [#171](https://github.com/dobesv/luchta/issues/171) — Retry task once on worker crash
- **Related Solution:** [worker-crash-handle-cache-dead-reuse-2026-06-13.md](worker-crash-handle-cache-dead-reuse-2026-06-13.md) — Dead-handle eviction and crash diagnostics
- **Related Solution:** [worker-crash-diagnostics-ownership-boundary-2026-06-20.md](../integration-issues/worker-crash-diagnostics-ownership-boundary-2026-06-20.md) — Crash detection flow
