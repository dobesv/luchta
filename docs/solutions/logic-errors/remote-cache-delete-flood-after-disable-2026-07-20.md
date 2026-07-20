---
title: "Remote cache delete flood after run-wide disable — missing guard in push path"
date: 2026-07-20
category: logic-errors
problem_type: logic_error
component: luchta-cache/shared/remote
root_cause: "Single queue-induced timeout tripped run-wide disable; client-side timeout covered queue-wait, not just I/O. Concurrency burst overwhelmed rclone daemon."
resolution_type: code_fix
severity: medium
tags:
  - remote-cache
  - rclone
  - circuit-breaker
  - defense-in-depth
  - mutation-testing
plan_ref: shared-cache-delete-flood
---

## Problem

Remote cache delete operations continued flooding logs with repeated 30s timeouts after the run-wide `disabled` flag was set, wasting wall-clock time and spamming output. The remote was already flagged as unhealthy, but in-flight push attempts still issued doomed delete calls.

## Symptoms

```text
warning: remote cache disabled: sync timed out after 30s
warn: shared cache remote snapshot delete failed for commit=<c> file=<hash>.bincode: rclone operation timed out after 30s   (repeated many times for same commit/files)
```

- Same shard files appeared multiple times with identical timeout warnings
- Each delete blocked full 30s `default_timeout`
- Concurrent tasks pushing to same commit caused duplicate log lines
- Run did NOT fail — deletes are best-effort — but wasted time and cluttered logs

## Investigation Steps

1. Prow CI logs showed pattern: remote disabled once, but delete warnings continued.

2. Traced `RemoteState.disabled` in `remote.rs` — `Arc<AtomicBool>` shared across all `RemoteSync` clones. `record_remote_error` → `disable_with_warning` sets it on genuine health failures (timeouts, unavailable, non-404 HTTP).

3. Checked `push_store_artifacts`: entry guard `if self.is_disabled() { return; }` exists, but:
   - Subsumed-shard delete loop had no `is_disabled()` check
   - Helper `delete_remote_snapshot_file` had no early guard

4. Many concurrent tasks call `push_store_artifacts` for SAME commit. Once one trips disable, others already past entry guard keep issuing deletes.

5. Confirmed: existing pull paths (`pull_snapshot_commit`, `pull_blob`) already use `is_disabled()` early-returns — push path incomplete.

## Root Cause

`push_store_artifacts` checked `is_disabled()` only at function entry. Its subsumed-shard delete loop and `delete_remote_snapshot_file` helper did NOT re-check the flag. In-flight pushes that passed the entry guard before disable continued firing deletes — each blocking 30s and logging. Concurrency (multiple tasks pushing same commit) explains duplicate lines for identical shard files.

The run-wide circuit-breaker pattern exists (`RemoteState.disabled`), but the push path lacked defense-in-depth guards at loop and helper boundaries.

## Solution

Added `is_disabled()` guards at three levels in the push path:

**1. After blob push (early exit):**
```rust
self.push_blob_if_missing(paths, outputs_hash);

if self.is_disabled() {
    return;
}
```

**2. After snapshot upload phase (conditional early return):**
```rust
if !uploaded_new_shard || self.is_disabled() {
    return;
}
```

**3. Inside subsumed-shard delete loop:**
```rust
for shard_id in &merge.subsumed_shard_ids {
    if self.is_disabled() {
        break;
    }
    self.delete_remote_snapshot_file(commit_key, shard_id, SNAPSHOT_FILE_EXTENSION);
    self.delete_remote_snapshot_file(commit_key, shard_id, SNAPSHOT_MERGED_EXTENSION);
}
```

**4. Helper-level guard in `delete_remote_snapshot_file`:**
```rust
fn delete_remote_snapshot_file(&self, commit_key: &str, shard_id: &str, extension: &str) {
    if self.is_disabled() {
        return;
    }
    // ... rest of delete logic
}
```

## Why This Works

The run-wide `Arc<AtomicBool>` acts as a circuit-breaker visible to all `RemoteSync` clones. Once set, no remote operations should proceed. Defense in depth:

- **Loop-level `break`**: PERFORMANCE optimization — avoids N no-op helper calls and potential extra rclone round-trips once disable trips mid-loop.
- **Helper-level guard**: CORRECTNESS guarantee — ensures any delete call after disable immediately returns, whether from loop or other callers.

Both guards needed: outer loop break prevents wasted calls; inner helper guard ensures correctness regardless of caller.

## Mutation Testing Insight

Removing the loop `break` did NOT make the regression test fail — the helper-level early-return prevents subsequent deletes after disable. Test validates observable behavior (surviving shard files remain untouched), which the helper guard enforces.

Lesson: When a run-wide circuit-breaker exists, guard at innermost shared helper (correctness) AND at loop/batch boundaries (perf). Mutation testing revealed which guard is load-bearing for correctness vs. optimization.

## Prevention Strategies

**Best Practices:**
- Audit all code paths that issue remote operations for `is_disabled()` guards, not just entry points.
- When a run-wide disable flag exists, check it at: (1) function entry, (2) before each I/O batch/loop iteration, (3) innermost shared helpers.
- Defense in depth: multiple guard layers protect against callers bypassing entry checks.

**Code Review Checklist:**
- [ ] Does this remote operation path check `is_disabled()` at entry?
- [ ] Do loops that call remote operations re-check the flag each iteration?
- [ ] Do shared helper functions have early-return guards?

**Testing:**
- Regression test poisons a remote file (directory instead of file) so delete fails and trips disable mid-loop, then asserts remaining shards untouched.
- Test gated behind `LUCHTA_TEST_RCLONE=1` — does not run in default CI. Known coverage limitation.

## Related Issues

- **Prior doc:** [integration-issues/s3-remote-cache-via-rclone-rcd-2026-06-19.md](../integration-issues/s3-remote-cache-via-rclone-rcd-2026-06-19.md) — initial rclone integration, established run-wide disable flag
- **Prior doc:** [performance-issues/remote-cache-parallel-sync-nested-runtime-2026-06-24.md](../performance-issues/remote-cache-parallel-sync-nested-runtime-2026-06-24.md) — startup latency fixes in remote layer
- **Plan:** `shared-cache-delete-flood`

---

## Round 3: Root Cause Fix

The earlier delete-guard fix treated a SYMPTOM. The real root cause: the rclone remote was NOT unreachable.

### Queue-Induced Timeouts

Under a burst of concurrent task-completion pushes (~97 tasks in parallel), many rclone ops hit the single `rclone rcd` daemon simultaneously. Each op is wrapped in `tokio::time::timeout(sync_timeout=30s, client.request(...))` — the timer covers QUEUE-WAIT + I/O, not just I/O. Ops that merely sat queued behind the burst tripped the 30s timeout even though the remote was healthy.

A SINGLE such `Timeout` tripped the run-wide disable flag, poisoning all caching for the entire run. No client-side concurrency cap existed on the push/delete path (pull path was bounded).

### Fix: Two Pragmatic Levers

**1. Timeout = transient backpressure, not instant kill:**

`RemoteState` now tracks consecutive timeouts. A `Timeout` only disables after K consecutive (default 8, env `LUCHTA_SHARED_CACHE_TIMEOUT_DISABLE_THRESHOLD`). Any successful op resets the streak.

- Genuine unreachability (`RemoteUnavailable`, `Process`, non-404 `HttpStatus`, `Rc`, `Request`, `Decode`, `Io`) still disables immediately.
- 404 + missing-local-source 500 stay non-disabling.
- Threshold clamped `.max(1)`.

```rust
fn record_timeout(&self) {
    let streak = self.consecutive_timeouts.fetch_add(1, Ordering::AcqRel) + 1;
    if streak >= self.timeout_disable_threshold {
        self.disable_with_warning("consecutive timeouts exceeded threshold");
    }
}
```

**2. Client-side concurrency cap on all rclone ops:**

`OpLimiter` — a `std::sync::Mutex` + `Condvar` counting semaphore with RAII `OpPermit`. Acquired on the CALLING thread before `runtime.block_on(...)`.

- Default 16 (env `LUCHTA_SHARED_CACHE_RCLONE_CONCURRENCY`), clamped `.max(1)`.
- Correct primitive: each op runs its own independent `block_on` on a scoped OS thread, so `tokio::sync::Semaphore` would NOT compose.

```rust
fn acquire(&self) -> OpPermit {
    let mut state = self.state.lock().unwrap();
    while state.in_flight >= self.max_in_flight {
        state = self.cvar.wait(state).unwrap();
    }
    state.in_flight += 1;
    OpPermit { limiter: Arc::clone(&self.limiter) }
}
```

### Design Insights

1. **Timeout as saturation signal, not health signal:** When a client-side timeout wraps queue time at a shared single-threaded-ish backend, timeouts indicate saturation — not unreachable. Distinguish "saturated/backpressure" (threshold, stay enabled) from "unreachable" (disable now).

2. **Bound producer concurrency at client:** Prevents enqueuing more than backend services — cheaper than tuning timeouts alone.

3. **Correct concurrency primitive matters:** Thread-blocking `Condvar` semaphore for sync-over-`block_on` fan-out; `tokio::Semaphore` only within a single async runtime.

### Deferred Follow-ups

- **Oracle Priority 1:** rclone rc `_async=true` + `job/status` polling — separates short submit-timeout from longer execution-timeout. Cleanest semantic fix.
- **Background push queue:** Fire-and-forget push off the task-completion critical path.
- **rcd tuning:** `--transfers`, `--checkers`, `--rc-job-expire-duration`.
- **Pre-existing flaky tests:** Parallel test isolation race in `shared::git` / luchta-cli suites (TempDir/cwd contention) — unrelated, separate follow-up.

---

## Notes

Pre-existing flaky test: `cargo test -p luchta-cache` has parallel test isolation race (TempDir/current_dir collision under `git.rs` Command::new). Unrelated to this fix. Passes single-threaded or under nextest process isolation.
