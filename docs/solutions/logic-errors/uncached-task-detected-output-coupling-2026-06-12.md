---
title: "Uncached task detected-output coupling for downstream cache invalidation"
date: 2026-06-12
category: logic-errors
problem_type: logic_error
component: luchta-cli/run
root_cause: "Inconsistent output pattern precedence between cache-write and uncached-dependency record paths"
resolution_type: code_fix
severity: high
tags:
  - cache-invalidation
  - output-patterns
  - detected-outputs
  - worker-protocol
  - jsonl-ipc
  - tokio-lifetime
plan_ref: luchta-build-cache
---

## Problem

Uncached tasks (`cache:false`) were recording only their declared `outputs` when computing the output hash used to invalidate downstream cached tasks. When workers emitted detected outputs via `DETECTED` in the `done` message, these were ignored for uncached dependencies, causing downstream `cache:true` tasks to use stale hashes and skip re-execution when the detected outputs changed.

## Symptoms

- Downstream cached tasks not re-running after uncached dependency's detected outputs changed
- `cache_uncached_dependency_output_change_reruns_downstream` test passed (declared outputs), but real-world scenarios with worker-emitted detected outputs silently failed
- No error messages — pure soundness gap where cache invalidation was incomplete

Example: A `lib#build` task with `cache:false` runs a worker that emits `detected-output.txt` as a `DETECTED` output (not declared in config). Downstream `app#build` with `cache:true` depends on `^build`. When `detected-output.txt` content changes, `app#build` should re-run but didn't because the output hash only covered declared outputs.

## Investigation Steps

1. Reviewed `cache_uncached_dependency_output_change_reruns_downstream` test — confirmed it only exercised declared outputs.
2. Traced cache-write path: `persist_cache_state` uses `effective_output_patterns(outcome)` which prefers detected over declared outputs.
3. Traced uncached-dependency record path: `record_resolved_output_hash` used `OutputHashRecordContext.output_patterns` initialized from task definition's declared `outputs` only — no detected-output override.
4. Identified invariant: ALL paths computing output hashes must apply same precedence (detected > declared) or downstream invalidation silently breaks.
5. Examined `spawn_task_runner` signature — found `'static` tokio task constraint prevented capturing `&outcome` post-run.
6. Designed fix: `OutputHashRecordContext::with_effective_patterns(outcome)` to override patterns inside the spawned task after run completes.

## Root Cause

The `OutputHashRecordContext` was initialized with declared outputs from the task definition and never updated. When an uncached task completed, `record_resolved_output_hash` used these declared patterns while the cache-write path used `effective_output_patterns` (detected > declared). This inconsistency meant:

1. Cached tasks wrote output hashes using detected outputs
2. Uncached dependencies recorded output hashes using declared outputs only
3. Downstream cached tasks compared against wrong hash when dependency was uncached -> stale cache hits

Additionally, a `printf` in the test worker's shell script corrupted the JSONL stream (stdout pollution), surfacing as `WorkerError::Crashed`.

## Solution

### 1. Add `with_effective_patterns` to `OutputHashRecordContext`

```rust
impl OutputHashRecordContext {
    /// Returns the context with its output patterns overridden by the worker's
    /// detected outputs when present, mirroring `effective_output_patterns`
    /// used by the cache-write path so uncached-dependency coupling hashes the
    /// same outputs a cached task would.
    fn with_effective_patterns(mut self, outcome: Option<&TaskRunOutcome>) -> Self {
        if let Some(detected) = outcome.and_then(|o| o.detected_outputs.clone()) {
            self.output_patterns = detected;
        }
        self
    }
}
```

### 2. Apply override inside spawned task post-run

```rust
// In spawn_task_runner, after executor.run() completes:
let output_hash_record = output_hash_record
    .map(|record| record.with_effective_patterns(outcome_res.as_ref().ok()));
```

### 3. Fix test worker stdout pollution

Shell test worker pipes task command's raw stdout into JSONL stream. Commands must write to files only:

**Before (broken):**
```sh
printf '%s\n' "$value"  # writes to stdout -> corrupts JSONL
```

**After (fixed):**
```sh
cp value.txt out.txt    # file-only, no stdout
```

### 4. Add regression test

```rust
#[test]
fn cache_uncached_detected_dependency_output_change_reruns_downstream_then_skips_when_stable() {
    // Worker emits detected-output.txt, downstream app reads it
    // Edit value.txt -> lib re-runs -> app re-runs (detected output changed)
    // Second run -> app skips (hash stable)
}
```

## Why This Works

1. **Consistent precedence**: Both cache-write and uncached-dependency paths now use `effective_output_patterns(detected > declared)`, ensuring downstream hashes compare equivalent outputs.

2. **Lifetime compliance**: Building `OutputHashRecordContext` pre-spawn with declared outputs (owned data) satisfies `'static`. Overriding with detected outputs inside the task (after `outcome_res` available) avoids capturing borrowed orchestrator state.

3. **Test worker isolation**: Writing only to files prevents stdout pollution of JSONL stream, eliminating `WorkerError::Crashed`.

## Prevention Strategies

### Test Cases

- Add tests for ALL paths that compute output hashes: cache-write, uncached-dependency record, cache-skip check
- Test worker-emitted detected outputs with uncached dependency and cached downstream
- Stress test with `--stress-count` to verify hash stability

### Best Practices

- **Consistent precedence**: Any path computing output hashes MUST apply `effective_output_patterns` precedence (detected > declared). Audit all uses of `TaskRunOutcome.detected_outputs`.
- **Lifetime pattern**: When a value depends on post-execution data but must live in a `'static` tokio task, build the owned context pre-spawn with fallback, then override inside the task.
- **Test worker stdout hygiene**: Shell test worker commands MUST write to files only. Any stdout_POLLUTION corrupts JSONL stream.

### Code Review Checklist

- [ ] All output hash computation paths use consistent output pattern precedence?
- [ ] Detected outputs applied to uncached-dependency record path?
- [ ] Tokio task doesn't capture borrowed orchestrator state?
- [ ] Test worker commands write to files (never stdout)?

## Related Issues

- **Related Solution:** [resident-worker-process-management-2026-06-09.md](../integration-issues/resident-worker-process-management-2026-06-09.md) — JSONL protocol liveness invariant, `Done` emission requirement
- **Related Solution:** [worker-trait-harness-extraction-2026-06-11.md](../integration-issues/worker-trait-harness-extraction-2026-06-11.md) — Protocol tests as behavior guards
- **Plan:** `luchta-build-cache` — Full implementation history including verification note 77bb5d08 and decision note c0e75035
