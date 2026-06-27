---
title: "Weight clamping and config validation gap for zero max_weight"
date: 2026-06-27
category: logic-errors
problem_type: logic_error
component: luchta-engine/executor, luchta-types/config
root_cause: "Task weight exceeding executor max rejected instead of clamped; config-path validation gap for zero max_weight"
resolution_type: code_fix
severity: medium
tags:
  - weight-clamping
  - semaphore
  - config-validation
  - input-path-coverage
  - executor
plan_ref: issue-140-clamp-weight
---

## Problem

Task weight exceeding executor's `max_weight` was rejected with `ExecutorError::WeightExceedsMax`, blocking valid oversized tasks instead of clamping and running. Additionally, config-file `concurrency.maxWeight: 0` bypassed CLI validation, creating a zero-permit semaphore that allowed unbounded concurrency.

## Symptoms

- Tasks with `weight > max_weight` failed with `ExecutorError::WeightExceedsMax`
- Config `maxWeight: 0` silently created zero-permit semaphore
- `acquire_many_owned(0)` succeeds immediately (Tokio semaphore semantics), bypassing concurrency limits
- CLI `--max-weight 0` rejected correctly, but config-path slipped through

## Investigation Steps

1. Reviewed issue #140 requirement: oversized tasks should clamp and run
2. Traced `validate_weight()` call site in `dispatch_loop` — errored before semaphore acquisition
3. Verified semaphore constructed with `max_weight` permits — acquiring `min(weight, max_weight)` always within capacity
4. Found CLI validation in argument parser but no validation in `ConcurrencyConfig` deserializer
5. Tested `acquire_many_owned(0)` — succeeds immediately, returns zero-permit guard
6. Integration tests surfaced worker crash (exit 141/SIGPIPE) when task command wrote to stdout — see Learnings

## Root Cause

**Weight rejection**: `validate_weight()` returned error when `task.weight > max_weight`. Issue #140 requires clamping instead.

**Config validation gap**: `ConcurrencyConfig::max_weight` had no serde validation. CLI rejected `--max-weight 0`, but `maxWeight: 0` in config file created a zero-permit semaphore. Tokio's `acquire_many_owned(0)` succeeds immediately (zero permits needed), allowing unbounded task execution.

**Input-path asymmetry**: Same setting validated on CLI path but not config-file path. Validation must cover all input paths or be defensive at construction.

## Solution

### Weight Clamping (executor.rs)

Replaced `validate_weight()` error with `effective_weight()` returning clamped value:

```rust
fn effective_weight(&self, task: &Task) -> u32 {
    task.weight.min(self.max_weight)
}
```

Used at semaphore acquisition:

```rust
let permits = self.semaphore
    .acquire_many_owned(self.effective_weight(&task))
    .await?;
```

Removed `ExecutorError::WeightExceedsMax` variant entirely. Clamping is silent — no warning — per product decision.

### Config Validation (config.rs)

Added custom serde deserializer to reject zero at deserialization:

```rust
pub fn deserialize_nonzero_max_weight<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = u32::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("maxWeight must be greater than 0"));
    }
    Ok(value)
}

#[derive(Deserialize)]
pub struct ConcurrencyConfig {
    #[serde(deserialize_with = "deserialize_nonzero_max_weight", ...)]
    pub max_weight: u32,
}
```

## Why This Works

**Weight clamping**: Semaphore constructed with `max_weight` permits. Acquiring `min(weight, max_weight)` always stays within capacity — no deadlock or starvation. Oversized tasks run at clamped weight, reducing effective parallelism but never exceeding configured limit.

**Config validation**: Deserializer rejects zero before executor sees it. All config-file paths now covered. If zero somehow reaches executor (e.g., default fallback), semaphore with zero permits would deadlock — better to fail fast at deserialization.

## Learnings

1. **Semaphore clamping is safe**: Acquiring fewer permits than requested is valid. The semaphore only bounds total acquired permits; acquiring `min(weight, max_weight)` when `weight > max_weight` simply grants fewer permits than the task "requested" but stays within total capacity.

2. **Validation must cover ALL input paths**: CLI validation alone is insufficient when the same setting also comes from config file. Fix belongs at deserialization (or defensively in constructor) so every path is covered. Pattern: validate at the point where the value enters the system, not at one specific caller.

3. **Test-fixture pitfall (worker protocol vs stdout)**: The `common::shell_worker` fixture speaks JSONL protocol over stdout. A task `command` writing to stdout (e.g., `echo a`) corrupts the stream, causing worker to die with SIGPIPE (exit 141) once the task runs. This only surfaced because previous behavior errored before task execution, hiding the fixture bug. Fix: use commands with no stdout (e.g., `true`) when test only needs task to run-and-succeed. See also [cache-persistence-decoupling-worker-protocol-2026-06-19.md](../logic-errors/cache-persistence-decoupling-worker-protocol-2026-06-19.md) for same class of issue.

## Prevention Strategies

**Test Cases:**
- Engine test: `weight > max_weight` runs with clamped effective weight
- Config deserializer test: `maxWeight: 0` rejected with error
- Integration test: oversized task under `maxWeight=1` runs (not errors)
- CLI test: `--max-weight 0` rejection still works

**Code Review Checklist:**
- [ ] For any config setting, are ALL input paths validated? (CLI, config file, defaults)
- [ ] Does validation happen at deserialization or construction, not just one caller?
- [ ] When changing error paths to success paths, do integration tests still exercise the runtime behavior?

**Related:**
- [cache-persistence-decoupling-worker-protocol-2026-06-19.md](cache-persistence-decoupling-worker-protocol-2026-06-19.md) — Same stdout-protocol fixture pitfall
- [uncached-task-detected-output-coupling-2026-06-12.md](uncached-task-detected-output-coupling-2026-06-12.md) — stdout pollution corrupts JSONL

## Related Issues

- **GitHub:** [#140](https://github.com/dobesv/luchta/issues/140) — Task weight exceeding max_weight should clamp instead of error
