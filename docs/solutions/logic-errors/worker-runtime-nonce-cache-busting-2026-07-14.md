---
title: "Worker runtime nonce: cache invalidation on worker version changes"
date: 2026-07-14
category: logic-errors
problem_type: logic_error
component: luchta-worker/resolve-protocol
root_cause: "task_spec_hash excludes worker runtime identity, so worker version bumps do not invalidate cached task results"
resolution_type: code_fix
severity: medium
tags:
  - cache-invalidation
  - backward-compatibility
  - serde-flatten
  - integration-test-isolation
  - cache-nonce
plan_ref: worker-version-cache-nonce
---

## Problem

Worker version upgrades and runtime configuration changes did not invalidate cached task results. Task cache keys (`task_spec_hash`) incorporated only config-level nonces, not worker-supplied runtime identity. When a worker binary was upgraded or its runtime config changed, existing cache entries remained valid, potentially serving stale results.

## Symptoms

- Tasks cached with worker version `1.0.0` remained valid after worker upgrade to `1.1.0`
- No mechanism for workers to signal "my identity changed" at runtime
- Users had no automatic recovery path from worker-bug-induced cache poisoning
- Integration tests using `std::env::set_var` for `LUCHTA_CACHE_NONCE` caused flaky failures in sibling tests

## Investigation Steps

1. Reviewed existing 4-scope cache nonce architecture (`env`, `global`, `worker`, `task`) in prior solution doc `cache-nonce-invalidation-control-2026-06-23.md`
2. Identified gap: `worker` scope populated from config only (`WorkerDefinition.cache.cache_nonce`), not runtime
3. Designed 5th scope `workerNonce` as additive extension, preserving backward compatibility
4. Investigated `ResolveResult` serialization: struct was `#[serde(transparent)]` single-field wrapper
5. Tested serialization shapes: plain `pub decision` field nests incorrectly as `{"decision":{"decision":"accept"}}`
6. Found test isolation bug in `cache_nonce_e2e.rs`: process-global env mutation leaked into child processes

## Root Cause

1. **Missing runtime nonce scope**: Cache nonce pipeline had no path for worker-supplied runtime identity to reach `task_spec_hash`

2. **Serde transparent limitation**: `ResolveResult` was `#[serde(transparent)]` wrapper around `ResolveDecision`. Adding optional field required removing transparent and using `#[serde(flatten)]` on the decision field to preserve wire shape `{"decision":"accept"}`

3. **Test isolation pitfall**: `env_nonce_busts_cache` test mutated process-global `LUCHTA_CACHE_NONCE` via `std::env::set_var`, which propagated to sibling test child processes via environment inheritance

## Solution

### 1. Extended Cache Nonce to 5 Scopes

Added `workerNonce` scope appended LAST in nonce string:

```
env=X&global=X&worker=X&task=X&workerNonce=X
```

Ordering critical: appending preserves substring match for existing 4-scope keys, so old cache entries remain valid.

### 2. Backward-Compatible ResolveResult Wire Format

Changed `ResolveResult` from transparent wrapper to struct with flattened decision:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolveResult {
    #[serde(flatten)]
    pub decision: ResolveDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_nonce: Option<String>,
}
```

Key serde techniques:
- `#[serde(flatten)]` on `decision` preserves old shape `{"decision":"accept"}`
- `#[serde(default)]` lets older workers omit `cache_nonce` → deserializes to `None`
- `#[serde(rename_all = "camelCase")]` matches worker protocol convention → field serializes as `cacheNonce`
- `skip_serializing_if` keeps wire compact when nonce absent

### 3. Worker Trait Default Method

Added default method returning `None`:

```rust
fn cache_nonce(&self) -> Option<String> {
    None
}
```

7 worker crates override with package version:
```rust
fn cache_nonce(&self) -> Option<String> {
    Some(env!("CARGO_PKG_VERSION").to_owned())
}
```

**Known limitation**: `luchta-lazy-worker` has no `impl Worker` block—it constructs `ResolveResult::accept()` directly in its main loop. Tasks behind lazy-worker do not receive runtime nonce invalidation.

### 4. Graph Storage and CLI Threading

- `TaskGraph.worker_nonces: HashMap<TaskId, String>` stores resolved nonces (private field, accessor only)
- `DecisionContext.resolve_task_nonce()` looks up `task_graph.worker_nonce(task_id)`
- `CacheNonceScopes::for_task()` helper assembles all 5 scopes
- Both cache-skip and cache-write paths use same resolver → single source of truth

### 5. Test Isolation Fix

Changed from process-global env mutation to child-scoped env:

```rust
fn run_luchta(temp: &TempDir, run: LuchtaRun<'_>) -> Assert {
    let mut cmd = Command::cargo_bin("luchta").unwrap();
    if let Some(nonce) = run.env_nonce {
        cmd.env("LUCHTA_CACHE_NONCE", nonce);
    } else {
        cmd.env_remove("LUCHTA_CACHE_NONCE");  // Key fix
    }
    // ...
}
```

**Rule**: never use `std::env::set_var` in parallel integration tests; pass env per-`Command` via `.env()` / `.env_remove()`.

## Why This Works

1. **Flatten preserves wire shape**: `#[serde(flatten)]` inlines the decision field rather than nesting it, so `{decision: Accept}` stays `{"decision":"accept"}` not `{"decision":{"decision":"accept"}}`

2. **Optional with default = backward compat**: older workers' JSON without `cacheNonce` deserializes correctly; field defaults to `None` and produces unchanged cache keys

3. **Scope ordering = stable keys**: appending `workerNonce` last means existing cache entries (without that scope) remain valid—no mass invalidation on upgrade

4. **Single-resolver pattern**: both read and write cache paths call same `resolve_task_nonce()` → nonce always consistent for same task

5. **Child-scoped env = isolated tests**: `.env_remove()` per-Command prevents ambient process env from leaking into sibling tests

## Prevention Strategies

### Test Cases

- Backward-compat deserialization: old JSON `{"decision":"accept"}` → `ResolveResult::accept()` with `cache_nonce: None`
- Runtime nonce change causes cache miss: same task, different worker nonce → different `task_spec_hash`
- Absent nonce produces same hash as before: `None` → no `workerNonce` scope in string
- Test isolation: multiple concurrent tests with different env nonces do not interfere

### Code Review Checklist

- [ ] `#[serde(flatten)]` used when extending transparent structs?
- [ ] Optional new fields have `#[serde(default)]` for backward compat?
- [ ] New nonce scopes appended LAST to preserve existing key substrings?
- [ ] Integration tests use `.env()`/`.env_remove()` per-Command, not `std::env::set_var`?
- [ ] Workers without `impl Worker` documented as exclusion?

### Key Correctness Traps

1. **Serde flatten vs transparent**: `transparent` wraps; `flatten` inlines. Adding fields to transparent struct requires removing transparent and flattening existing field(s).

2. **Process-global env in tests**: rust integration tests run concurrently by default. `std::env::set_var` is process-wide and leaks to child processes via inheritance. Always scope env per-Command.

3. **Scope ordering**: cache nonce is a string, not a map. Appending new scopes preserves substring match for old keys; inserting in middle breaks them.

4. **Cargo test vs nextest**: `cargo test --workspace` runs tests in single binary with shared cwd; `cargo nextest` runs process-per-test. Pre-existing flaky tests may pass under nextest but fail under cargo test. Branch didn't touch flaky component; defer fix to separate issue.

## Related Issues

- **GitHub**: [#227](https://github.com/dobesv/luchta/issues/227) — Worker version cache nonce
- **Related Solution**: [cache-nonce-invalidation-control-2026-06-23.md](./cache-nonce-invalidation-control-2026-06-23.md) — 4-scope nonce architecture (prior art)
- **Related Solution**: [worker-reports-schema-migration-2026-06-21.md](./worker-reports-schema-migration-2026-06-21.md) — Bincode schema migration precedent (avoided here)
- **Related Solution**: [env-control-cache-correctness-single-resolver-2026-06-16.md](./env-control-cache-correctness-single-resolver-2026-06-16.md) — Single-resolver pattern
