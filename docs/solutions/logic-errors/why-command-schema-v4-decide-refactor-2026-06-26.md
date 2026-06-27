---
title: "Why command with persisted run_reason: bincode positional schema migration and decide() refactor"
date: 2026-06-26
category: logic-errors
problem_type: logic_error
component: luchta-cache/decide
root_cause: "bincode positional encoding limits schema evolution; decide() refactor changed resolver-error semantics and downstream dep_outputs timing"
resolution_type: code_fix
severity: high
tags:
  - bincode
  - schema-migration
  - decide
  - run_reason
  - dep_outputs
  - resolver-error
  - OnceLock
  - MSRV
plan_ref: luchta-why-command
---

## Problem

Added `luchta why -p <pkg> <task>` to explain task execution decisions, requiring: (1) persisted `run_reason` in TaskRunRecord (schema V4), (2) single `decide()` reason path shared by execution and live analysis. Two subtle regressions emerged during integration: downstream tasks reran on unchanged builds (local skip failed to record outputs_hash), and resolver errors stopped forcing reruns (refactored decide() returned Skip instead of Run).

## Symptoms

```text
# Regression 1: cache_root_task_output_change_reruns_downstream failed
- First run: app#build recorded dep_outputs={"#build": [hash]}
- Second run (unchanged): #build skipped correctly, but app#build reran
- app reason: "DepOutputMismatch { tasks: ["#build"] }" (phantom)
- Downstream saw empty current_dep_outputs={}, no upstream hash recorded

# Regression 2: cache_resolve_error_writes_empty_outputs_record_and_warns failed  
- First run: invalid output glob "missing[*" → warning emitted, empty outputs recorded
- Second run: decide() returned Skip/CacheHit instead of Run
- No rerun, no warning on resolver error
```

## Investigation Steps

Started by tracing `dependency_output_hashes` flow: reads from `ctx.output_hashes` map populated by `record_output_hash`. Original `try_cache_skip` recorded skip hash via `record_output_hash(..., prior.outputs_hash)`. Refactor moved recording into `build_task_run_context` Skip arm — unreachable because `handle_cache_skip` returns before `build_task_run_context` runs.

Next traced resolver-error path: original `patterns_unchanged` checked `resolve_outputs(inputs) == Err` → returned `false` (changed) → Run. Refactored `input_changed_reason` returned `None` on Err → fell through to Skip.

Both regressions visible in git diff: `handle_cache_skip` lost its `record_output_hash` call, and `input_changed_reason` inverted error semantics.

## Root Cause

**Regression 1 — dep_outputs staleness:**
`handle_cache_skip` (dispatch.rs L243-250) sent completion without recording the skipped task's `outputs_hash` into `ctx.output_hashes`. Downstream's `dependency_output_hashes()` found no entry → empty `dep_outputs` → `decide()` saw mismatch against prior's populated `dep_outputs` → false `DepOutputMismatch`.

**Regression 2 — resolver-error inversion:**
`input_changed_reason` (decide.rs L278-288) inverted error handling:
```rust
let Ok(resolved_entries) = resolved_entries else {
    return None;  // ❌ WRONG: treats error as unchanged
};
```
Old `patterns_unchanged` returned `false` on Err (changed → Run).

## Solution

### 1. Bincode V4 Schema Migration

Appended `run_reason: Option<RunReason>` as LAST field in TaskRunRecord:

```rust
pub struct TaskRunRecord {
    // ... existing fields (positional order must be preserved) ...
    pub cache_nonce: Option<String>,  // Added in V3
    pub run_reason: Option<RunReason>, // Added in V4 (LAST)
}
```

Version gate in store.rs:
```rust
pub fn read(&self, task_id: &str) -> Option<TaskRunRecord> {
    let bytes = fs::read(self.task_dir(task_id).join("meta.bincode")).ok()?;
    let (record, _): (TaskRunRecord, usize) =
        bincode::serde::decode_from_slice(&bytes, bincode_config()).ok()?;
    // Only accept V4 records. V1/V2/V3 → None → clean cache miss.
    (record.schema_version == SCHEMA_VERSION_V4).then_some(record)
}
```

**Bincode trap:** `#[serde(default)]` is INEFFECTIVE for bincode. Positional encoding with fixed-int means fields match positionally, not by name. Old bytes missing the trailing field fail decode → `None`. One-time cache invalidation on upgrade, acceptable and documented.

Tested both:
- Genuine V3 bytes (missing run_reason) → decode fails → None
- V4 bytes with `schema_version: 3` → decode succeeds but version gate rejects → None

### 2. Single decide() Reason Path

Refactored `decide()` to return `DecisionResult { action, reason }`:

```rust
pub struct DecisionResult {
    pub action: Decision,
    pub reason: RunReason,  // Always populated, even for Skip/CacheHit
}

pub fn decide(prior: Option<&TaskRunRecord>, current: &CurrentState<'_>) -> DecisionResult {
    let Some(prior) = prior else {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::NoPriorRecord,
        };
    };

    if !prior.succeeded {
        return DecisionResult {
            action: Decision::Run,
            reason: RunReason::PriorFailed,
        };
    }

    // Precedence: NonceChanged > TaskSpecMismatch > DepOutputMismatch >
    //             PkgDepMismatch > EnvMismatch > InputChanged > CacheHit
    // Short-circuits on first match
}
```

`Option<RunReason>` exists ONLY on record (for old records). `decide()` ALWAYs returns a reason.

### 3. Fix: Skip Output Hash Recording

Added hash recording in `handle_cache_skip` before completion:

```rust
fn handle_cache_skip(...) {
    match decision.action {
        Decision::Skip => {
            ctx.reporter.task_skipped_cache_hit(task_id);
            // FIX: Record prior hash for downstream dep_outputs
            if let Some(prior) = ctx.cache.read(&task_id.to_string()) {
                record_output_hash(ctx.output_hashes, task_id, prior.outputs_hash);
            }
            done_tx.take().expect("...").send(()).ok();
        }
        // ...
    }
}
```

### 4. Fix: Resolver Error Forces Run

Changed `input_changed_reason` to return `Some(reason)` on Err:

```rust
fn input_changed_reason(...) -> Option<RunReason> {
    let resolved_entries = match kind {
        FileEntryKind::Inputs => resolver.resolve_inputs(patterns),
        FileEntryKind::Outputs => resolver.resolve_outputs(patterns),
    };
    let Ok(resolved_entries) = resolved_entries else {
        // FIX: Resolver error → force Run
        return Some(RunReason::InputChanged {
            changed: Vec::new(),
            truncated: false,
            change_count: 0,
        });
    };
    // ...
}
```

Added unit test: `decide_resolver_error_forces_run`.

### 5. Fix: why Dep_outputs from Cache

For live "now" decision in `why`, built dep_outputs from cached records:

```rust
fn build_dep_outputs_from_cache(
    task_id: &TaskId,
    prepared: &PreparedWorkspace,
    cache: &Cache,
) -> BTreeMap<String, [u8; 32]> {
    prepared.task_graph
        .dependencies_of(task_id)
        .into_iter()
        .filter_map(|dep| {
            let dep_id_str = dep.id.to_string();
            let record = cache.read(&dep_id_str)?;
            Some((dep_id_str, record.outputs_hash))
        })
        .collect()
}
```

Empty map would cause phantom `DepOutputMismatch` for every task with dependencies.

### 6. MSRV: OnceLock over LazyLock

Project MSRV is 1.78; `std::sync::LazyLock` requires 1.80. Use `OnceLock` (1.70):

```rust
fn empty_task_env() -> &'static BTreeMap<String, EnvSpec> {
    static C: OnceLock<BTreeMap<String, EnvSpec>> = OnceLock::new();
    C.get_or_init(BTreeMap::new)
}
```

## Why This Works

**Bincode version gate:** Positional encoding means any field mismatch (missing trailing field) causes decode failure → `None`. Cache miss triggers re-execution, producing fresh V4 record. One-time cost.

**Skip hash recording:** `ctx.output_hashes` is the single source of truth for downstream `dep_outputs`. Recording on skip ensures downstream sees upstream hash whether upstream ran or skipped.

**Resolver error → Run:** When resolver cannot determine file state, safest action is re-execution. Returning `None` on error meant invalid globs silently skipped.

**Cached dep_outputs for why:** Live decision mirrors runtime: fetch each dependency's current output hash from its cached record (or omit if uncached).

## Prevention Strategies

**Test Cases:**
- V3 bytes → read returns None (decode failure)
- V4 bytes with `schema_version: 3` → read returns None (version gate)
- Resolver error → decide returns Run
- Downstream task with skipped upstream → dep_outputs populated from prior.outputs_hash
- Task with dependency → why reports "up to date" (not DepOutputMismatch)
- `some(Option<String>)` added to bincode struct invalidates existing caches once

**Code Review Checklist:**
- [ ] New field appended as LAST position in bincode struct?
- [ ] Schema version constant updated in record.rs AND store.rs?
- [ ] Read path checks version AFTER decode?
- [ ] Resolver error → Run (not Skip)?
- [ ] Skip path records output hash before downstream branch?
- [ ] Static helper uses OnceLock (1.70), not LazyLock (1.80)?

**Key Correctness Traps:**

1. **Bincode positional != serde(default):** Adding `Option` field to bincode struct changes layout even for `None`. Decoding old bytes fails. Gate on `schema_version` after decode.

2. **Refactoring boolean → Option inverts error semantics:** Original `changed?` returned `false` on error (force run). New `reason?` returning `None` on error means unchanged. Preserve error-path force-run semantics.

3. **Skip path unreachable for execution refactor:** If `handle_cache_skip` returns before `build_task_run_context`, recording moved into the latter never executes. Keep state updates near the decision point.

4. **why dep_outputs from cache not empty:** Live "now" decision needs dependency hashes. For no-execution why, source from prior records, not empty map.

## Related Issues

- **GitHub:** [#136](https://github.com/dobesv/luchta/issues/136) — Why command for task execution analysis
- **Related Solution:** [worker-reports-schema-migration-2026-06-21.md](./worker-reports-schema-migration-2026-06-21.md) — Prior bincode schema migration
- **Related Solution:** [cache-nonce-invalidation-control-2026-06-23.md](./cache-nonce-invalidation-control-2026-06-23.md) — V3 schema migration pattern
