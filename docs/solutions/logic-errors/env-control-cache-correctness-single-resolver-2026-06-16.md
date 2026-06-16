---
title: "Environment variable control: single-resolver correctness for cache and execution"
date: 2026-06-16
category: logic-errors
problem_type: logic_error
component: luchta-types, luchta-cache, luchta-cli
root_cause: "Divergent env resolution between execution and cache hashing caused stale-cache bugs when default fallback was added"
resolution_type: code_fix
severity: high
tags:
  - cache-invalidation
  - env-resolution
  - single-source-of-truth
  - strict-env
  - hash-boundary
plan_ref: luchta-env-control
---

## Problem

Environment variable resolution for task execution and cache hashing diverged, creating stale-cache bugs when `EnvSpec.default` fallback was introduced. A `default` value change would affect execution but not hashing, causing incorrect cache hits. Additionally, resident workers inherited stale ambient environment from launch time, and built-in passthrough whitelist vars could pollute the cache hash, reducing cache portability across machines.

## Symptoms

```
- Cache hit returned stale results after changing EnvSpec.default value
- resolv_env_value test failures (default not considered in hashing)
- Resident workers had different env than fresh workers (ambient env drift)
- PATH changes invalidated caches on different machines (whitelist in hash)
```

## Investigation Steps

Started by reviewing existing hash boundary doc (`hash-boundary-task-spec-vs-separate-2026-06-12.md`) which established that `task_spec_hash` must stay env-free; `env_hash` is the sole authority for env in cache key.

Traced env resolution paths:
- Execution: `resolve_task_env` in `luchta-cli/src/run.rs` builds `HashMap<String, String>` for `ExecutionRequest.env`
- Hashing: `env_hash` in `luchta-cache/src/hashing.rs` hashed only `spec.value`, ignoring `spec.default`

Found that `EnvSpec` already had `value` and `input` fields; `default` was being added. The naive implementation updated execution to use default fallback but forgot hashing, creating the divergence.

Earlier iteration (task b6b8d918 initial) added `EnvSpec::resolve_env_value` but only wired it into execution. Argus verification (note `6b9ed849`) caught this: hashing still had duplicated inline resolution logic that didn't call the shared resolver.

## Root Cause

**Two separate resolution logic paths**: `env_hash` had its own inline `match spec.value { Some => ... None => resolver(name) }` that didn't include the new `default` field. This meant:
- Changing `default: "foo"` to `default: "bar"` would change task execution but NOT change `env_hash`
- Cache key would match, serving stale cached output

**Resident worker env drift**: Workers spawned via `command.envs(&request.env)` inherited full ambient env. Long-lived workers (pool pattern) would have stale ambient env from launch time.

**Whitelist in hash**: If `env_hash` were called with full `ExecutionRequest.env` (including whitelist passthrough), machine-specific vars like `PATH` would cause cache misses across machines.

## Solution

### 1. Single Shared Resolver

Added `EnvSpec::resolve_env_value<F>(&self, _name: &str, ambient: F) -> Option<String>` in `luchta-types/src/lib.rs`:

```rust
impl EnvSpec {
    /// Resolves the effective environment value for this specification.
    ///
    /// This is the **single authority** for environment value resolution.
    /// Both task execution and cache hashing must use this function.
    ///
    /// # Precedence
    ///
    /// 1. If `self.value` is `Some`, use it (including empty string as present).
    /// 2. Else, if `ambient(name)` returns `Some`, use that (inherited).
    /// 3. Else, if `self.default` is `Some`, use it.
    /// 4. Else, return `None` (omit the variable).
    pub fn resolve_env_value<F>(&self, _name: &str, ambient: F) -> Option<String>
    where
        F: FnOnce() -> Option<String>,
    {
        if let Some(ref v) = self.value {
            return Some(v.clone());
        }
        if let Some(v) = ambient() {
            return Some(v);
        }
        self.default.clone()
    }
}
```

Both execution and hashing now call this method:
- `luchta-cli/src/run.rs:1518`: `spec.resolve_env_value(name, || std::env::var(name).ok())`
- `luchta-cache/src/hashing.rs:33`: `spec.resolve_env_value(name, || resolver(name))`

### 2. Merged Env for Hashing (Not ExecutionRequest.env)

`build_current_state` in `cache_ctx.rs` receives `merged_env: &BTreeMap<String, EnvSpec>` (the declared env specs), NOT the execution env. Hashing operates on EnvSpec entries, which:
- Preserve `input` flag for opt-out filtering
- Exclude built-in passthrough whitelist (which isn't declared)
- Include `default` field for resolution

```rust
pub fn build_current_state<'a>(
    task_def: &'a TaskDefinition,
    merged_env: &'a BTreeMap<String, EnvSpec>, // <- merged spec, not execution HashMap
    // ...
) -> CurrentState<'a> {
    CurrentState {
        task_spec_hash: task_spec_hash(task_def),
        // Whitelist passthrough is NOT in merged_env, so excluded from hash
        env_hash: env_hash(merged_env, |name| std::env::var(name).ok()),
        // ...
    }
}
```

### 3. Strict Subprocess Environment

Both spawn paths now `env_clear()` before setting env:

```rust
// luchta-worker/src/runtime.rs:194 (worker path)
command.env_clear();
command.envs(&request.env);

// luchta-engine/src/executor.rs:416 (direct path)
command.env_clear();
command.envs(&request.env);
```

CLI builds full effective env in `build_execution_env`:
1. Collect present-only whitelist vars from ambient env
2. Overlay resolved declared vars (declared wins on collision)
3. Send full `HashMap<String, String>` in `WorkerRequest.env`

Worker only receives the final env map — never reads its own ambient env.

### 4. Hash Boundary Preserved

`task_spec_hash` continues to exclude env (per `hash-boundary-task-spec-vs-separate-2026-06-12.md`). The `TaskSpecHashInput` struct comment explicitly states:

```rust
// `env` is deliberately excluded here — it is tracked by `env_hash`,
// which honors the `input: false` opt-out.
```

### 5. Scope Precedence: Task > Worker > Global

`merge_env` in `luchta-cli/src/env_merge.rs`:

```rust
pub(crate) fn merge_env(
    global: &BTreeMap<String, EnvSpec>,
    worker: Option<&BTreeMap<String, EnvSpec>>,
    task: &BTreeMap<String, EnvSpec>,
) -> BTreeMap<String, EnvSpec> {
    let mut merged = global.clone();
    if let Some(worker) = worker {
        merged.extend(worker.clone());
    }
    merged.extend(task.clone());
    merged
}
```

Per-key override: later scope wins. `TaskDefinition.env` is never mutated — merge produces a new map.

### 6. Conflict Detection (Check Only, Not Run)

`env_conflict.rs` detects `value.is_some() && default.is_some()` within a single scope. Reported only in `luchta check`. `luchta run` does not error on this (undefined runtime behavior accepted).

## Why This Works

**Hashed == Executed**: The single `resolve_env_value` method is the authority. Both paths use identical precedence logic. Empty string is handled consistently (present value, not coerced to unset).

**Cache Portability**: Builtin passthrough whitelist (`BUILTIN_PASSTHROUGH_ENV` in `luchta-worker/src/lib.rs`) is 35 vars: PATH, HOME, proxy vars (both cases), etc. These are injected into `ExecutionRequest.env` but never appear in `merged_env`, so `env_hash` never sees them. Changing `PATH` on a different machine doesn't bust the cache.

**No Resident Worker Drift**: Workers start with cleared env and receive full effective env from CLI. Pooling workers don't accumulate stale ambient env.

**Backward Compatibility**: `EnvSpec.default` uses `#[serde(default)]`. Existing configs without `default` work unchanged.

## Prevention Strategies

### Test Cases

- `env_hash` must call `resolve_env_value` (not duplicate logic)
- Changing `EnvSpec.default` invalidates cache
- Empty string in `value` is hashed as present (not as unset)
- `input: false` var changes do not invalidate cache
- Whitelist-only ambient changes (PATH) do not invalidate cache
- `task_spec_hash` excludes env (regression from prior boundary)
- Scope merge: task > worker > global per-key
- `luchta check` reports `value + default` conflict; `luchta run` doesn't error
- `env_clear()` before `envs()` on both spawn paths

### Best Practices

- **Single-resolver pattern**: When multiple code paths need to compute the same value (hashing, execution, validation), create ONE resolver function with clear precedence documentation. All callers use it.
- **Hash boundary**: Keep `env_hash` and `task_spec_hash` separate. Fields with opt-out semantics (`input: false`) belong in `env_hash` only.
- **Stale-cache prevention**: When adding a new fallback/derivation to env resolution, verify BOTH execution AND hashing are updated. Run `cargo test --workspace` after any change to shared resolution logic.
- **Strict subprocess isolation**: Always `env_clear()` before `envs()` for deterministic env. Never rely on ambient env in worker processes.
- **Merge, don't mutate**: Store merged config in a new map; never mutate original `TaskDefinition.env`.

### Code Review Checklist

- [ ] Does env hashing call `resolve_env_value` (single authority)?
- [ ] Does execution call `resolve_env_value` (same resolver)?
- [ ] Is `env_clear()` called before `envs()` on all spawn paths?
- [ ] Is built-in whitelist excluded from `env_hash`?
- [ ] Is `TaskDefinition.env` not mutated?
- [ ] Does `task_spec_hash` still exclude env?
- [ ] Are new optional fields serde-defaulted for backward compatibility?

## Related Issues

- **GitHub:** [#21](https://github.com/dobesv/luchta/issues/21) — Environment variable control for tasks
- **Related Solution:** [hash-boundary-task-spec-vs-separate-2026-06-12.md](./hash-boundary-task-spec-vs-separate-2026-06-12.md) — Hash boundary preservation
- **Plan Note:** `730e7822` — Design for `env_hash` wiring (merged_env, not execution HashMap)
- **Plan Note:** `6b9ed849` — Verification caught initial hashing divergence (execution-only resolver)
