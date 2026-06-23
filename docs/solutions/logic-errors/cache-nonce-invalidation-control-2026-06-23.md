---
title: "Cache nonce: user-controlled invalidation to escape poisoned cache entries"
date: 2026-06-23
category: logic-errors
problem_type: logic_error
component: luchta-cache/hashing
root_cause: "task_spec_hash excludes worker code/version, so poisoned cache entries survive worker bug fixes without a user escape hatch"
resolution_type: code_fix
severity: high
tags:
  - cache-invalidation
  - hashing
  - cache-nonce
  - spec-hash
  - schema-migration
  - single-resolver
plan_ref: cache-nonce-118
---

## Problem

Stale cache entries survive worker bug fixes because `task_spec_hash` excludes worker code and version. When a worker bug is fixed (e.g., reporting more inputs), existing cache entries remain valid and prevent re-execution. Users had no manual escape hatch to force cache invalidation for specific tasks.

## Symptoms

```
- Worker bug fixed → cached output still used → incorrect results persist
- No config knob to force rebuild of specific tasks
- Cache poisoning requires manual cache-clear command, losing all cache benefit
- GitHub #118: need per-task invalidation mechanism without global cache clear
```

## Investigation Steps

1. Analyzed cache key composition: `task_spec_hash` + `env_hash` + `input_hash` + `output_hash`.
2. Worker code/version not in spec hash by design — would cause rebuild on any worker change.
3. Determined need for user-controlled salt that folds into spec hash.
4. Evaluated placing nonce in `env_hash` (rejected — env has `input: false` opt-out semantics).
5. Cross-referenced prior art: `hash-boundary-task-spec-vs-separate-2026-06-12.md` — nonce belongs in `task_spec_hash` because it has no opt-out; always invalidates.

## Root Cause

Cache identity based solely on task definition, env, and file hashes. No mechanism for users to signal "this task's cached output is poisoned, force rebuild" without clearing the entire cache. Worker implementations can change without cache invalidation, leaving incorrect cached outputs.

## Solution

### 1. Four-Scope Nonce Configuration

Added `cacheNonce` field to `CacheConfig`, reused at three config scopes:
- **Global**: `LuchtaConfig.cache.nonce`
- **Worker**: `WorkerDefinition.cache.nonce`
- **Task**: `TaskDefinition.cache.nonce`
- **Env var**: `LUCHTA_CACHE_NONCE` (independent 4th source, busts all caches)

All four sources **combine** (not override). Combined into keyed sparse string:

```
env=<v>&global=<v>&worker=<v>&task=<v>
```

Only-present keys included; all-absent → `None` → backward-compatible hashes unchanged.

### 2. Hash Boundary: Nonce in `task_spec_hash`

```rust
// hashing.rs: TaskSpecHashInput
pub struct TaskSpecHashInput<'a> {
    pub command: &'a str,
    pub worker: Option<&'a str>,
    pub weight: Option<u32>,
    pub depends_on: &'a [String],
    pub cache_enabled: bool,
    pub inputs: &'a [String],
    pub outputs: &'a [String],
    pub nonce: Option<&'a str>,  // Added after outputs
}
```

Nonce belongs in `task_spec_hash`, NOT `env_hash`:
- Nonce has no opt-out semantics (unlike `env: { input: false }`)
- Any supplied value must always invalidate task-spec identity
- Cross-ref: `hash-boundary-task-spec-vs-separate-2026-06-12.md`

### 3. Single Shared Resolver for Read/Write Paths

**Critical correctness invariant**: Read path (`try_cache_skip`) and write path (`build_cache_write_context`) must resolve identical nonce or cache NEVER hits.

```rust
// DispatchContext helper used by both paths
pub fn resolve_task_nonce(&self, task_def: &TaskDefinition) -> Option<String> {
    resolve_cache_nonce(
        self.env_cache_nonce.as_deref(),    // LUCHTA_CACHE_NONCE
        self.global_cache_nonce.as_deref(), // LuchtaConfig.cache.nonce
        worker_nonce,                        // sparse lookup via workers map
        task_def.cache.as_ref()?.nonce.as_deref(),
    )
}
```

Worker nonce lookup is **sparse**: `task_def.worker` → `workers.get(w)` → `cache.nonce`. Missing/dangling worker → worker nonce absent (not an error).

### 4. Percent-Encoding for Collision Safety

```rust
// cache_nonce.rs
const ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b'%')
    .add(b'&')
    .add(b'=');

// Prevents: "a&b" colliding with separate "env=a&global=b" encoding
```

Values containing `&`, `=`, or `%` are percent-encoded to prevent delimiter forgery.

### 5. Schema Migration: V3 with Graceful Miss

Added `cache_nonce: Option<String>` to `TaskRunRecord`:

```rust
pub struct TaskRunRecord {
    // ... existing fields ...
    pub schema_version: u32,        // V3
    pub cache_nonce: Option<String>, // Added at end
}
```

Store gates reads to V3 only:

```rust
fn read(path: &Path) -> Option<TaskRunRecord> {
    let record: TaskRunRecord = bincode::decode_from_slice(...).ok()?;
    (record.schema_version == SCHEMA_VERSION_V3).then_some(record)
}
```

V1/V2 records → `None` → clean cache miss (no panic).

### 6. One-Time Upgrade Invalidation Accepted

Adding `Option<String>` to bincode struct changes layout even for `None` (discriminant byte). Existing caches invalidate once on upgrade. This is acceptable and documented — users already rebuilding after upgrade.

## Why This Works

1. **Nonce in spec hash**: Any nonce change produces different `task_spec_hash`, forcing cache miss.
2. **Combine semantics**: Changing any of four sources changes combined string → different hash.
3. **Single resolver**: Both read/write paths call `ctx.resolve_task_nonce(task_def)` → same nonce used for cache lookup and write.
4. **Sparse worker lookup**: Tasks without workers or with unknown worker refs gracefully omit worker nonce (no panic).
5. **Schema gate**: Old records fail version check → cache miss → task reruns → fresh V3 record written.
6. **Percent-encoding**: Delimiter collision prevented; values can't forge key boundaries.

## Prevention Strategies

### Test Cases

- **Hash changes**: `task_spec_hash(None)` != `task_spec_hash(Some("v1"))` != `task_spec_hash(Some("v2"))`
- **Combine not override**: Changing any scope changes combined nonce
- **Read/write identity**: `try_cache_skip` and `build_cache_write_context` resolve same nonce
- **Sparse worker**: Missing/dangling worker → worker nonce absent, not error
- **Old schema miss**: V1/V2 record → `None`, not panic
- **E2E integration**: Same nonce ⇒ cache hit; changed nonce ⇒ miss/rerun

### Code Review Checklist

- [ ] Nonce resolver called by BOTH read and write cache paths?
- [ ] Env var read ONCE per dispatch context, not per task?
- [ ] Worker lookup uses `.and_then()` chain returning `None` (no panic)?
- [ ] Percent-encode set includes `CONTROLS` + `%` + `&` + `=`?
- [ ] Schema version constant updated in record.rs AND store.rs AND sample fixtures?
- [ ] All-absent nonce → `None` → backward-compat hash preserved?

### Key Correctness Traps

1. **Read/write resolver divergence** — The #1 trap. If read path and write path resolve different nonces, cache NEVER hits. Use single shared resolver.
2. **Local cache is single-slot** — `Cache::read/write` keeps only ONE record per `task_id`. Reverting nonce is fresh miss, not restoration of prior entry. Only shared cache keeps multiple candidates.
3. **Option discriminant byte** — bincode serializes `Option` discriminant even for `None`. Adding `Option` field invalidates existing caches once.
4. **Workspace clippy for shared-types changes** — Adding field to `WorkerDefinition` requires `cargo clippy --workspace --all-targets` to catch test-only struct literals, not just `-p luchta-types`.

### Monitoring

- Warn if `LUCHTA_CACHE_NONCE` contains invalid UTF-8 (currently silent)
- One-time upgrade notice if V1/V2 records detected in cache directory

## Related Issues

- **GitHub**: [#118](https://github.com/dobesv/luchta/issues/118) — Cache nonce for user-controlled invalidation
- **Related Solution**: [hash-boundary-task-spec-vs-separate-2026-06-12.md](./hash-boundary-task-spec-vs-separate-2026-06-12.md) — Hash boundary philosophy
- **Related Solution**: [env-control-cache-correctness-single-resolver-2026-06-16.md](./env-control-cache-correctness-single-resolver-2026-06-16.md) — Single-resolver pattern for env
