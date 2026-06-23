---
title: "Yarn worker done-time inputs clobbered declared patterns (cache invalidation break)"
date: 2026-06-22
category: logic-errors
problem_type: logic_error
component: luchta-worker/yarn-worker
root_cause: "WorkerRequest lacked declared inputs/outputs, so yarn worker's done_response rebuilt from scratch instead of editing, clobbering task's declared input patterns"
resolution_type: code_fix
severity: high
tags:
  - cache-invalidation
  - detected-patterns
  - worker-protocol
  - replace-semantics
  - yarn-worker
  - jsonl-ipc
plan_ref: luchta-yarn-worker-inputs-clobbering
---

## Problem

Yarn worker reported only `package.json` as a task's done-time inputs. The engine treats worker-reported done-time inputs/outputs as REPLACE semantics (not merge), so this clobbered the task's declared inputs (e.g. `src/**/*.ts`), breaking cache invalidation — edits to source files no longer busted the build cache.

## Symptoms

```
$ luchta logs --show-inputs build
Inputs: package.json    # WRONG — should include src/**/*.ts

# Editing src/index.ts does NOT trigger rebuild
# Cache hits incorrectly on stale declared input patterns
```

GitHub issue #117. Config declared `inputs: ["src/**/*.ts"]` but worker response replaced it with `["package.json"]` only.

## Investigation Steps

1. Ran `luchta logs --show-inputs build` — saw only `package.json` despite config declaring `src/**/*.ts`
2. Checked engine's replace semantics in `effective_input_patterns` (dispatch.rs) — confirmed worker-reported inputs REPLACE declared patterns
3. Traced yarn worker's `done_response` — found it calls `detected_inputs_with_package_json(None)` because `WorkerRequest` had no declared inputs field
4. Realized the worker had no way to augment declared inputs; it could only rebuild from scratch

## Root Cause

`WorkerRequest` did not carry the task's declared inputs/outputs. Yarn worker's `done_response` method received only command/env metadata, so it hardcoded `detected_inputs_with_package_json(None)` — returning only `package.json` and discarding the task's actual declared patterns.

The engine's `effective_input_patterns` / `effective_output_patterns` apply REPLACE semantics when `detected_input_patterns` or `detected_output_patterns` flags are set. If a worker returns detected inputs, those completely replace declared inputs.

## Solution

**Pattern: "Edit, don't reconstruct."**

Thread declared metadata into the worker request so workers can AUGMENT (not replace) task metadata.

### 1. Add declared inputs/outputs to request structs

**ExecutionRequest (luchta-engine):**
```rust
pub struct ExecutionRequest {
    // ...existing fields...
    pub declared_inputs: Option<Vec<String>>,
    pub declared_outputs: Option<Vec<String>>,
}
```

**WorkerRequest (luchta-worker):**
```rust
pub struct WorkerRequest {
    // ...existing fields...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_inputs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_outputs: Option<Vec<String>>,
}
```

Wire-compatible JSONL: `skip_serializing_if="Option::is_none"` means old engine versions (that don't send these fields) work with new workers, and new engines work with old workers (that ignore unknown fields).

### 2. Populate from task_def (luchta-cli dispatch.rs)

```rust
fn build_command_map(...) {
    // ...
    declared_inputs: Some(task_def.inputs.clone()),
    declared_outputs: Some(task_def.outputs.clone()),
}
```

### 3. Yarn worker merges instead of replacing

```rust
fn done_response(&self, req: &WorkerRequest, ...) -> Option<DoneResponse> {
    let declared = req.declared_inputs.clone().unwrap_or_default();
    let mut merged: BTreeSet<String> = declared.into_iter().collect();
    merged.insert("package.json".to_string());
    
    Some(DoneResponse {
        detected_inputs: Some(merged.into_iter().collect()),
        detected_outputs: req.declared_outputs.clone(),
    })
}
```

Use `BTreeSet` for deterministic dedup/ordering — stable cache keys.

### 4. Default Worker::done_response keeps returning None

Keep the base implementation returning `None` so the engine falls back to declared patterns. Echoing request fields there would falsely mark all workers as reporting detected I/O.

## Why This Works

1. **Augment semantics**: Worker receives declared inputs, merges detected `package.json`, returns union. No clobbering.

2. **Wire compatibility**: `Option<Vec<String>>` with serde defaults means mixed engine/worker versions interoperate. Old engine → worker sees `None`, falls back to detected-only. Old worker → engine ignores unknown fields.

3. **Deterministic merge**: `BTreeSet` ensures stable ordering, reproducible cache hashes.

4. **Default safety**: Base `Worker::done_response` returns `None`, triggering engine's declared-pattern fallback. Only workers that explicitly need to augment I/O override this.

## Key Insight: Runtime vs Resolve-Time Modification

Two distinct phases:

| Phase | Mechanism | What it modifies |
|-------|-----------|------------------|
| Resolve-time | `TaskModification` / `Modify` | Command, `depends_on`, weight — structural edits |
| Run-time | `Done.inputs/outputs` | Effective I/O for cache hashing |

This fix is the run-time analogue of the resolve-time `TaskModification` mechanism. Both follow "edit, don't reconstruct" — workers modify specific fields while preserving the rest.

## Prevention Strategies

### Test Cases

- Worker that merges detected input with declared inputs — verify both present in `luchta logs --show-inputs`
- Worker that reports only declared outputs — verify clobbering doesn't happen
- Mixed engine/worker versions — verify wire compatibility
- BTreeSet ordering — verify cache hash stability across runs

### Best Practices

- **Thread declared data**: When a protocol uses replace-on-report semantics, send declared values into workers so they can EDIT (merge/augment) rather than RECONSTRUCT.
- **Option with serde defaults**: New protocol fields should be `Option<T>` with `#[serde(default, skip_serializing_if="Option::is_none")]` for backward compatibility.
- **BTreeSet for merged sets**: When combining detected + declared patterns, use `BTreeSet` for deterministic ordering.

### Code Review Checklist

- [ ] Does worker receive declared inputs/outputs before computing done response?
- [ ] Does worker MERGE detected patterns with declared (not replace)?
- [ ] Does default `done_response` return `None` (not echo request fields)?
- [ ] Are new JSONL protocol fields `Option` with serde defaults?
- [ ] Does merge use `BTreeSet` for determinism?

## Related Issues

- **GitHub:** [#117](https://github.com/dobesv/luchta/issues/117) — Original report
- **Related Solution:** [detected-patterns-flag-conflation-2026-06-12.md](./detected-patterns-flag-conflation-2026-06-12.md) — Replace semantics for detected patterns
- **Related Solution:** [uncached-task-detected-output-coupling-2026-06-12.md](./uncached-task-detected-output-coupling-2026-06-12.md) — Output pattern precedence consistency
