---
title: "Task failure fast-stop: first-failure latch, collateral classification, and shared-worker serialization gotcha"
date: 2026-06-23
category: logic-errors
problem_type: logic_error
component: luchta-cli/run
root_cause: "Race conditions in failure handling, collateral vs genuine failure misclassification, and test invalidation due to shared-worker serialization"
resolution_type: code_fix
severity: high
tags:
  - concurrency
  - atomic-operations
  - compare_exchange
  - worker-management
  - fast-stop
  - testing
  - integration-tests
plan_ref: luchta-run-failure-handling
---

## Problem

Implementing `luchta run --continue` and default fast-stop-on-failure behavior exposed three non-obvious concurrency and testing pitfalls: (1) ensuring exactly-once failure-triggered shutdown, (2) distinguishing genuine failures from collateral (fast-stop-killed) tasks, and (3) a shared-worker serialization gotcha that invalidated integration tests.

## Symptoms

- Integration test `default_mode_fast_stop_exit_code_nonzero` took 10+ seconds instead of <1s, appearing to show fast-stop not working
- Flaky test `logs_filters_failed` — collateral tasks leaked into `--failed` output ~40-60% failure rate under parallel runs
- Progress stats showed phantom pending tasks after failures; wave completion stalled (`🌊 N/M` never reached `N/N`)

```text
# Shared-worker test timing symptom
default_mode_fast_stop_terminates_in_flight_worker_promptly elapsed=10.078s
# Expected: <5s (long task sleeps 30s, should be killed)

# Flaky logs output
╭─ app#failure · ...
╰─ 0.0s · exit 1 · cache <hash>
no cached output for app#success  # <- collateral task leaked
```

## Investigation Steps

1. **Shared-worker timing mystery**: Test put `fastfail#fail` and `longrun#build` on the same `shell` worker. Shell worker reads stdin line-by-line and blocks per command — tasks on one worker are serialized. Scheduling sent longrun's run message first → `sleep 10` completed before fastfail ran → no in-flight task to kill. Control experiment with two distinct workers (`w1`, `w2`) proved fast-stop works correctly (exits in 0.25s).

2. **Collateral leak diagnosis**: `logs --failed` enumerated requested tasks and printed "no cached output" for any without a record. Fast-stop-killed siblings didn't persist a FAILED record (correctly suppressed via `interrupted` flag), but still appeared in task selection.

3. **Latch design**: Multiple failure paths (invalid-task, cache-expansion, runtime) could each set `any_failed`. Needed exactly-once shutdown trigger without double-kill.

## Root Cause

### 1. Shared-Worker Serialization Gotcha (Test Design Flaw)

Workers process `run` messages serially. A shell worker reading stdin line-by-line blocks inside each command. Two "independent" tasks on the same worker never run concurrently. Integration tests asserting concurrent in-flight behavior must use distinct workers per task.

### 2. Latch Race Condition

Original design had failure paths directly calling `shutdown_immediate()`. If the first failure's shutdown was deferred (async spawn not yet scheduled), subsequent failures could race to trigger shutdown again, or the gate-check `if any_failed.load()` might not see the flag yet.

### 3. Collateral vs Genuine Misclassification

The existing `interrupted` atomic was reused for fast-stop signaling. Collateral tasks (killed by fast-stop) should be reported as uncounted and skip cache record persistence. The key invariant: `persist_failure_record = succeeded || !interrupted`. Only genuine first failure should count and persist.

## Solution

### 1. First-Failure Latch Pattern

Single `compare_exchange` is the source of truth for both flipping `any_failed` AND triggering shutdown:

```rust
fn trigger_fast_stop_on_first_failure(
    any_failed: &Arc<AtomicBool>,
    interrupted: &Arc<AtomicBool>,
    continue_on_failure: bool,
    worker_manager: &Arc<WorkerManager>,
) -> bool {
    let first_failure = any_failed
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();

    if first_failure && !continue_on_failure {
        interrupted.store(true, Ordering::SeqCst);
        let worker_manager = Arc::clone(worker_manager);
        tokio::spawn(async move {
            worker_manager.shutdown_immediate().await;
        });
    }

    first_failure
}
```

Key properties:
- `compare_exchange(false, true)` returns `Ok(())` exactly once — the "latch winner"
- Only latch-winner sets `interrupted` and spawns shutdown
- Returns bool so callers know if they were first
- Shutdown is spawned async but flag-flip is synchronous, so spawn gate sees it immediately

### 2. Collateral Classification

```rust
// Evaluated while persisting cache state, BEFORE finalize flips the latch.
let persist_failure_record = succeeded || !interrupted.load(Ordering::SeqCst);

// In cache state logging
if !interrupted.load(Ordering::SeqCst) {
    eprintln!("{} {}", "✖".red(), expansion_error.red());
}
```

Ordering is what makes this correct: `persist_failure_record` is read during
cache persistence, which happens *before* `finalize_task_run` runs the latch and
sets `interrupted`. So:

- Genuine first failure: at persist time `interrupted` is still `false`, so its
  failure logs print and its cache record IS persisted. The latch then flips
  `interrupted=true` afterwards.
- Collateral siblings: they finalize *after* the latch has already set
  `interrupted=true`, so at their persist time `interrupted` is already `true` —
  logs suppressed, cache record skipped, and they are reported uncounted (not failed).
- `--continue` mode: the latch never sets `interrupted` (no fast-stop), so every
  failed task counts normally and persists its record.

### 3. Integration Test Fix

Tests asserting concurrent in-flight behavior must:

```rust
// WRONG: both tasks on same worker — serialized, no concurrency
fastfail#fail -> worker w1
longrun#build -> worker w1  // runs AFTER fastfail completes

// CORRECT: distinct workers per task
fastfail#fail -> worker w1, command "sleep 0.5; exit 1"
longrun#build -> worker w2, command "sleep 30"
// Now failing task runs concurrently with long-running task
```

Additionally, the failing task must fail AFTER the long task starts (hence `sleep 0.5; exit 1` not instant `exit 1`), ensuring there's a genuine in-flight worker to terminate.

## Why This Works

### Latch Pattern

`compare_exchange` is an atomic test-and-set. It atomically checks `any_failed == false` and sets it to `true`. Only one caller succeeds (returns `Ok`), guaranteeing exactly-once shutdown spawn. The synchronous flag-flip before spawning prevents races where spawn-gate checks might miss the flag.

### Collateral Classification

Reusing the existing `interrupted` atomic (also used by Ctrl-C handling) creates a shared "stop signal". Fast-stop sets it just like SIGINT. Cache persistence and log output check this flag — if set, the task was collateral-killed, not genuinely failed. This avoids leaking fast-stop collateral into `--failed` views or progress counts.

### Test Fix

Distinct workers mean distinct subprocess PIDs. Each worker reads its own stdin, so commands run truly concurrently. The wall-clock timing assertion (`< 10s` while long task sleeps 30s) proves fast-stop killed the in-flight worker promptly.

## Prevention Strategies

### Test Cases

- **Concurrent in-flight assertion**: require >=2 distinct workers; fail instantly-long-running task starts first; assert wall-clock bound
- **Collateral suppression**: assert `logs --failed` output matches only tasks with genuine `exit != 0`
- **Latch exactly-once**: concurrent calls to `trigger_fast_stop_on_first_failure` — only one returns `true`

### Best Practices

- **Workers are units of concurrency**: never assume two tasks on one worker run in parallel
- **Sync flag-flip, async side-effect**: flip atomic synchronously, spawn async work after — prevents TOCTOU races
- **Reuse existing signals**: `interrupted` atomic for both Ctrl-C and fast-stop; collateral handling stays consistent

### Code Review Checklist

- [ ] Integration tests for concurrent dispatch use distinct workers per concurrent task?
- [ ] Failure-latch uses `compare_exchange` for exactly-once semantics?
- [ ] Collateral/fast-stop behavior tested separately from genuine failure?
- [ ] Progress accounting subtracts failed tasks from pending?

## Related Issues

- **GitHub:** [#101](https://github.com/dobesv/luchta/issues/101) — `--continue` flag
- **GitHub:** [#82](https://github.com/dobesv/luchta/issues/82) — Fast-stop on failure
- **Related Solution:** [async-shutdown-worker-pool-notify-race-2026-06-10.md](./async-shutdown-worker-pool-notify-race-2026-06-10.md) — graceful shutdown mechanism reused for fast-stop
- **Related Solution:** [noop-connector-task-exclusion-from-progress-stats-2026-06-22.md](./noop-connector-task-exclusion-from-progress-stats-2026-06-22.md) — progress counting patterns
