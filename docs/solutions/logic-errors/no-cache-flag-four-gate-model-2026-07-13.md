---
title: "No-cache flag: four-gate dispatch model separating bypass from cache-enabled"
date: 2026-07-13
category: logic-errors
problem_type: logic_error
component: luchta-cli/dispatch
root_cause: "Local skip gate alone insufficient; must also force Decision::Run in write-path to preserve local metadata under bypass"
resolution_type: code_fix
severity: high
tags:
  - cache-bypass
  - dispatch-gating
  - local-metadata
  - DecisionContext
  - watch-propagation
  - testing-traps
plan_ref: luchta-no-cache-flag
---

## Problem

Adding a cache-bypass flag (`--no-cache` / `LUCHTA_NO_CACHE`) requires four independent gates: (1) local skip bypass, (2) shared cache read bypass, (3) shared cache write bypass, and (4) local metadata write preservation. A naive implementation gating only the local skip check fails because the write-path (`build_cache_decision_context`) still returns `Decision::Skip` for unchanged inputs, causing `cache_write=None` and silently dropping local metadata updates.

## Symptoms

- `--no-cache` forces reruns but subsequent normal runs re-execute tasks that should skip
- E2E test `no_cache_flag_forces_rerun` fails at assertion that normal run after `--no-cache` skips
- Counter increments beyond expected value because local cache metadata was never refreshed
- Bug masked by stale `target/debug/luchta` binary — fresh `CARGO_TARGET_DIR` reveals the issue

```
// Expected: counter stays at 3 after normal run following two --no-cache runs
// Actual: counter=4 because local cache metadata not updated during --no-cache runs
assert_eq!(read_counter(&temp, "packages/app/counter.txt"), 3); // FAILS: got 4
```

## Investigation Steps

1. Verified dispatch skip-gate: `skip_enabled = cache_enabled && !ctx.no_cache` — tasks enter execution path under `--no-cache` ✓
2. Verified shared-read gate: `maybe_mark_shared_cache_hit` early-returns when `no_cache=true` — shared cache not consulted ✓
3. Verified shared-write gate: `shared_store_enabled = cache_enabled && !no_cache` — shared cache not written ✓
4. Traced write-path: `build_cache_decision_context(...)` still calls `decide()`, returns `Decision::Skip` for unchanged inputs
5. Found bug: `build_task_run_context` maps `Decision::Skip => None`, so `cache_write=None`, `persist_cache_state` skipped
6. Replayed with fresh binary: `CARGO_TARGET_DIR=/tmp/fresh cargo test` proved source-level bug vs stale artifact confusion

## Root Cause

The local skip gate (`skip_enabled`) only controls whether `try_cache_skip` is called. When `no_cache=true`:
- `skip_enabled=false` → `try_cache_skip` never called → task enters execution path ✓
- BUT `build_task_run_context` still calls `build_cache_decision_context` when `cache_enabled=true`
- `build_cache_decision_context` calls local `decide()` → returns `Decision::Skip` for unchanged inputs
- `build_task_run_context` maps `Skip => None` → `cache_write=None`
- `persist_cache_state` sees `None` → skips `write_run_record` → local metadata NOT refreshed

The write-path decision logic was unaware of the cache-bypass request. Gating at the skip-check level alone is insufficient; the decision must be forced to `Run` for cache-write purposes.

## Solution

Added three gates in `dispatch.rs` plus a decision-forcing fix:

**Gate 1: Local Skip Bypass (dispatch.rs:188)**
```rust
let skip_enabled = cache_enabled && !ctx.no_cache;
```
Controls `try_cache_skip` call. When `no_cache=true`, skip disabled, task enters execution.

**Gate 2: Shared Cache Read Bypass (dispatch.rs:1063)**
```rust
fn maybe_mark_shared_cache_hit(ctx, no_cache, cache_ctx, input, dep_outputs) {
    if no_cache || !matches!(input.decision.action, Decision::Run) {
        return;  // early return, no shared cache restore
    }
    // ... shared cache lookup ...
}
```

**Gate 3: Shared Cache Write Bypass (dispatch.rs:500)**
```rust
shared_store_enabled: cache_enabled && !no_cache,
```
Passed to `persist_cache_state`, gates the `shared.store()` call.

**Gate 4: Local Metadata Write (dispatch.rs:1030-1032) — THE FIX**
```rust
fn build_cache_decision_context(task_id, ctx, no_cache, cache_ctx) -> CacheDecisionContext {
    // ... read local cache, call decide() ...
    maybe_mark_shared_cache_hit(ctx, no_cache, cache_ctx, input, dep_outputs);
    
    // FORCE Decision::Run under no_cache to ensure cache_write is populated
    if no_cache {
        cache_ctx.decision = cache_run_decision();
    }
    cache_ctx.decision.clone()
}
```

This forces `Decision::Run` AFTER `maybe_mark_shared_cache_hit` (so shared read still bypassed) but BEFORE returning to `build_task_run_context` (so `cache_write=Some(...)`).

## Why This Works

1. **Four-gate model**: Each cache interaction point has an independent gate controlled by `no_cache`:
   - Skip-check gate: `skip_enabled = cache_enabled && !no_cache`
   - Shared-read gate: early return in `maybe_mark_shared_cache_hit`
   - Shared-write gate: `shared_store_enabled = cache_enabled && !no_cache`
   - Local-write gate: forced `Decision::Run` ensures `cache_write=Some`

2. **Decision forcing placement**: Forcing AFTER `maybe_mark_shared_cache_hit` preserves shared-cache bypass. Forcing BEFORE return ensures `build_task_run_context` sees `Decision::Run` and builds `cache_write` context.

3. **Separation of concerns**: `cache_enabled` (task-definition concern) stays unchanged. `no_cache` lives on `DispatchContext` (dispatch scope), NOT `DecisionContext`. This lets local metadata writes proceed normally while bypassing skip and shared cache.

4. **Watch propagation symmetry**: `WatchRunConfig.no_cache` propagates through both conversion sites (`driver.rs:635, 717`) into `RunCycleParams` — same pattern as `continue_on_failure`.

### Key Design Principle

**Do NOT force `cache_enabled = false` to implement bypass.**

If `cache_enabled` were forced false:
- `build_task_run_context` would skip `build_cache_decision_context` call entirely
- `cache_write` would be `None`
- Local metadata would NOT be written
- Subsequent normal runs would re-execute, violating spec

Correct separation: "cache enabled" = task definition concern; "cache bypass requested" = runtime execution policy on `DispatchContext`.

## Prevention Strategies

### Test Cases

- **Skip control under no_cache**: fixture task MUST be cache-enabled (`"cache":{}`) — inputs/outputs alone do NOT enable skip; `TaskDefinition::cache_enabled() = self.cache.is_some()`
- **Watch test rigor**: three cycles — (1) normal run, (2) normal skip control, (3) `no_cache=true` rerun — proves dispatch gate works
- **Full invariant chain**: E2E test `no_cache_flag_forces_rerun` asserts 5-step sequence: run→skip→no_cache_rerun→no_cache_rerun→normal_skip

### Testing Traps to Avoid

1. **Tautological watch tests**: Without `"cache":{}` in fixture, task is never cache-enabled, so skip check never happens — test passes trivially without exercising bypass logic.

2. **Stale binary masking bugs**: Default `target/debug/luchta` may be stale. Use fresh `CARGO_TARGET_DIR=/tmp/fresh-test` or `cargo clean -p luchta-cli` before E2E.

3. **Propagation completeness**: `no_cache` must be threaded at EVERY `continue_on_failure` propagation site. Grep for all conversion sites:
   - `watch/driver.rs`: two conversion sites (`CycleRequest` construction)
   - `run.rs`: `RunCycleParams`
   - `dispatch.rs`: `DispatchContext` and all runner spawn sites

4. **E2E assertions on counter**: Counter pattern requires declared input (`src.txt`) and output (`counter.txt`), git init, and `"cache":{}` config.

### Code Review Checklist

- [ ] `cache_enabled` computed from task definition ONLY (not modified by `no_cache`)
- [ ] `skip_enabled = cache_enabled && !ctx.no_cache` (separate from `cache_enabled`)
- [ ] `shared_store_enabled = cache_enabled && !no_cache` (write gate)
- [ ] `maybe_mark_shared_cache_hit` receives `no_cache` param and early-returns
- [ ] Decision forcing: `if no_cache { cache_ctx.decision = cache_run_decision(); }` AFTER shared-hit check
- [ ] All watch conversion sites propagate `no_cache`
- [ ] E2E test includes `--no-cache` → normal-run skip assertion (proves metadata persistence)
- [ ] Test fixtures include `"cache":{}` to enable cache

### Monitoring

- Alert if `--no-cache` runs followed by normal runs show unexpected re-execution (metadata persistence regression)

## Related Issues

- **GitHub**: [#123](https://github.com/dobesv/luchta/issues/123) — Option to disable cache and skip for a run
- **Related Solution**: [cache-nonce-invalidation-control-2026-06-23.md](./cache-nonce-invalidation-control-2026-06-23.md) — Nonce-based cache invalidation (different escape hatch)
- **Related Solution**: [env-control-cache-correctness-single-resolver-2026-06-16.md](./env-control-cache-correctness-single-resolver-2026-06-16.md) — Single-resolver pattern for env
