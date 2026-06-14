---
title: "Wave-bucketed progress reporter for Cargo workspace task runner"
date: 2026-06-13
category: workflow-issues
problem_type: workflow_issue
component: luchta-cli/progress-reporter
root_cause: "Per-task logging noise and missing diagnostics for CI OOM debugging"
resolution_type: code_fix
severity: medium
tags:
  - progress-reporting
  - wave-scheduling
  - concurrency
  - interrupt-handling
  - rss-monitoring
  - permit-gated-execution
plan_ref: luchta-issue-40-cleaner-build-output
---

## Problem

Large monorepo builds (732+ tasks) produced noisy, unreadable output with per-task start/finish logging. No diagnostics existed for CI OOM debugging. Running-task counts inflated because tasks were marked "running" before acquiring concurrency permits.

## Symptoms

- Hundreds of lines of per-task start/finish spam during builds
- Progress output unusable for large workspaces
- No visibility into memory consumption or signal handling during interrupt
- Running-count display showed inflated numbers (hundreds) when only `max_weight` tasks actually executed
- Cache-hit skips conflated with no-command/ordering-only skips
- Default concurrency bug masked: `maxWeight: 1` ran serially, but inflated running-count hid this

## Investigation Steps

1. Analyzed existing `dispatch_loop` in `run.rs` — per-task logging scattered through `spawn_task_runner`
2. Discovered `task_started` called before `executor.run()`, but permit acquired inside `run()` — tasks marked "running" while queued
3. Found `ConcurrencyConfig::default()` returned `max_weight: 1` — serial builds when config omitted `maxWeight`
4. Tested cache-skip accounting: no-command nodes not counted, causing wave totals never reaching 100%
5. Traced output capture: `ExecutionLogSink` only installed for cache-enabled tasks, allowing non-cache task output to leak
6. Shell-script fake workers in tests hung due to incorrect NDJSON protocol (`"success": true` instead of `"exitCode": N`)

## Root Cause

**Permit-gated execution misalignment**: Tasks marked as "running" at dispatch time, not permit-acquisition time. This inflated `running_count` to hundreds when only a few tasks held permits.

**Default concurrency**: `ConcurrencyConfig::default()` used `max_weight: 1`, causing fully serial execution when config omitted `maxWeight`. Accurate running-count fix exposed this bug.

**Skip accounting conflation**: No-command/ordering-only nodes not counted as "done", preventing wave progress from reaching 100%.

**Sink installation conditional**: `log_sink` installed only when `cache_write.is_some()`, leaking non-cache task output to terminal.

**Test harness brittleness**: Hand-rolled shell-script workers emitted wrong NDJSON fields, causing hangs in nextest.

## Solution

### 1. ProgressReporter with Wave-Bucketed Display

Created `crates/luchta-cli/src/progress.rs` implementing periodic, wave-bucketed progress:

```rust
pub struct ProgressReporter {
    pub wave_of: HashMap<TaskId, usize>,
    pub wave_done: Vec<AtomicUsize>,
    pub wave_skipped: Vec<AtomicUsize>,
    pub running: Mutex<HashMap<TaskId, Instant>>,
    done: AtomicUsize,
    skipped: AtomicUsize,
    pub mode: OutputMode,
    pub total_waves: usize,
    pub wave_total: Vec<usize>,
    pub start: Instant,
}
```

Key semantics:
- `task_ran`: Increments `done` (includes no-command/ordering-only nodes)
- `task_skipped_cache_hit`: Increments `skipped` (cache-hit ONLY)
- `task_finished_other`: Removes from running set, no counter change

Periodic progress line format (per cb329451):
```text
142/1850 done · 53 skipped · 12 running · 1643 pending · W3 18/493 · W4 4/343 · W5 2/149 · running: activity-report#build:browser, ajv#build:node, algolia#build:browser +9 · 36s · RSS 2.1 GB
```

- Aggregate counts: `done/total`, `skipped`/`running`/`pending` (latter two omitted when 0)
- Frontier waves: ONLY waves with running tasks (cap ~3), rendered `W{n} {done}/{total}`
- Running task list: strip common `@scope/` prefix when all share scope; group by scope when mixed
- Elapsed time and RSS at end

### 2. Permit-Gated Running Count

Added `WeightedExecutor::run_with_on_start()`:

```rust
pub async fn run_with_on_start<F>(
    &self,
    request: &ExecutionRequest,
    on_start: F,
) -> Result<TaskRunOutcome, ExecutorError>
where
    F: FnOnce(),
{
    self.validate_weight(&request.task)?;

    let permit = self.semaphore.clone()
        .acquire_many_owned(request.task.weight)
        .await
        .expect("executor semaphore closed unexpectedly");

    on_start();  // Called AFTER permit acquired

    // ... run task body ...
}
```

In `spawn_task_runner`:
```rust
executor.run_with_on_start(&request, {
    let reporter = Arc::clone(&reporter);
    move || reporter.task_started(&started_task_id)
}).await;
```

### 3. Fixed Default Concurrency

`ConcurrencyConfig::default()` now uses `available_parallelism()`:

```rust
impl Default for ConcurrencyConfig {
    fn default() -> Self {
        let max_weight = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        Self { max_weight }
    }
}
```

### 4. No-Command Task Accounting

Changed no-command branch in `dispatch_ready_task`:
```rust
// Before: task_finished_other(&task_id) — node never counted
// After:
ctx.reporter.task_ran(&task_id);
```

Now ordering-only nodes count toward `done`, enabling wave totals to reach 100%.

### 5. Always-Capture Sink, Print on Failure

`spawn_task_runner` now creates `ExecutionLogSink` for EVERY task:

```rust
let log_sink = ExecutionLogSink::new();
request.log_sink = Some(log_sink.clone());
// ... executor.run_with_on_start ...
if failed && !interrupted_run {
    print_captured_logs(&log_sink);
}
```

Cache persistence (`cache_write`) independent from capture (`log_sink`).

### 6. Process-Tree RSS Reader

`crates/luchta-cli/src/rss.rs`: Linux `/proc` BFS over `/proc/<pid>/task/<tid>/children`:

```rust
pub fn process_tree_rss_bytes() -> Option<u64> {
    // Start at self PID, sum VmRSS, walk children BFS
    // Non-Linux: returns None
}
```

- No `sysinfo` dependency
- Called on every periodic line AND interrupt line
- `format_rss(None)` returns `"unavailable"`

### 7. Interrupt Diagnostics

`shutdown_signal()` returns `ShutdownSignal` enum (`CtrlC`, `SigTerm`) with `name()` accessor. Interrupt branch prints:

```text
Interrupted by SIGTERM: 12 tasks running after 36s; RSS: 2.1 GB
```

## Why This Works

1. **Wave buckets are a read-only lens**: Walker releases tasks by dependency readiness, not wave barriers. Wave index precomputed via `compute_wave_indices()`. "Frontier waves" display shows waves with running tasks, making overlap readable.

2. **Permit-gated on_start**: `task_started` fires after semaphore acquisition, so `running_count` reflects actual executing tasks, not queued tasks. Accurate counts exposed the serial-default bug.

3. **Cache-hit-only skips**: Distinguishes cache hits from no-command/ordering-only/pruned skips. Total = done + skipped + running + pending.

4. **Sink for all tasks**: Captures output regardless of cache configuration. Preserves interrupt crash-output suppression (`!interrupted_run` gate).

5. **Scope stripping**: Monorepos rarely have package name collisions after dropping `@scope/` prefix. Mixed scopes grouped individually.

## Prevention Strategies

**Test Cases:**
- `wave_total_reaches_100_percent`: Include no-command nodes, verify wave completes
- `running_count_matches_max_weight`: Stress test with `maxWeight: 4`, assert `running <= 4`
- `cache_hit_counted_as_skipped_only`: Verify no-command/pruned NOT counted as skipped
- `output_captured_on_failure`: Assert captured logs print only on task failure

**Best Practices:**
- Increment running-count AFTER permit acquisition, not at dispatch
- Use typed worker message structs (`luchta-worker`), not shell-script heredocs
- Test with `cargo nextest` under stress (`--stress-count=3`) to expose flaky timing bugs
- Install log sinks universally; gate persistence separately

**Code Review Checklist:**
- [ ] `task_started` called after permit acquisition?
- [ ] Default concurrency tested with config omission?
- [ ] No-command nodes counted as done?
- [ ] log_sink installed for all tasks?
- [ ] Worker protocol uses `"exitCode": N` (not `"success": true`)?

## Test Harness Pitfalls

**Shell-script workers are brittle**: Hand-rolled NDJSON heredocs drift from protocol. Worker `done` message requires `"exitCode"` field. Wrong field = engine never resolves job → hang.

**Recommended**: Small compiled Rust test-worker binary reusing `luchta-worker` typed messages. One interrupt-diagnostic integration test remains `#[ignore]`d for shell-worker brittleness.

## Related Issues

- **GitHub:** [#40](https://github.com/dobesv/luchta/issues/40) — Cleaner build output
- **Related Solution:** [logic-errors/async-shutdown-worker-pool-notify-race-2026-06-10.md](../logic-errors/async-shutdown-worker-pool-notify-race-2026-06-10.md) — Graceful async shutdown preserving `ctx.interrupted` crash-output suppression
