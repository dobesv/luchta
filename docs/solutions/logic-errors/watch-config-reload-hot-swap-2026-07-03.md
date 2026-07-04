---
title: "Watch mode config hot-reload with CLI override preservation"
date: 2026-07-03
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: "Watcher only treated Create/Remove/Rename event kinds as structural (missing Modify(Data)); config-only edits with unchanged package set were no-ops; rebuild passed stale resolved max_weight instead of original CLI override"
resolution_type: code_fix
severity: medium
tags:
  - watch-mode
  - config-reload
  - ArcSwap
  - structural-change
  - hot-swap
  - cli-override
  - graceful-degradation
plan_ref: luchta-172-watch-config-reload
issue: "#172"
---

## Problem

Watch mode ignored content edits to the root `luchta-config.*` file. Config changes (e.g., updating `concurrency.maxWeight`, adding environment variables) required a manual restart of the watch session to take effect.

## Symptoms

- Editing `luchta-config.sh` or `luchta-config.mts` while watch mode ran caused no rebuild
- Watch log showed `[watch] up to date` even when config content changed
- Changing `maxWeight` in config had no effect on active session's semaphore
- Environment variable additions in config were not applied to subsequent task runs

## Investigation Steps

1. **Traced event classification**: `contains_structural_change` (`watcher.rs:259-276`) only checked event KIND via `is_structural_event_kind` — `Create`/`Remove`/Rename` => structural, `Modify(Data)` => not structural. A config content edit is `Modify(Data)`, so it bypassed the structural flag.

2. **Analyzed StructuralPackageSetDiff handling**: Driver's `run_one_iteration` (`driver.rs:462-469`) had explicit `Unchanged => {}` no-op branch. Even if structural flag were set, config-only edits that don't change discovered packages would be skipped.

3. **Discovered max_weight override bug during testing**: E2E tests revealed `WatchSession::rebuild_for_packages` called `load_config` (picking up new `maxWeight`) but passed `current.max_weight` to `build_reused_worker_run_context`. The helper stored this verbatim, discarding the reloaded config's `concurrency.maxWeight`. Result: config edits could reload the graph but never change config-derived scalars.

4. **Verified ArcSwap behavior**: Confirmed existing infrastructure already supported safe hot-swap: `rebuild_for_packages` preserves `WorkerManager` Arc, uses `ArcSwap` for atomic context swap, and has graceful degradation on error.

## Root Cause

Three separate issues combined:

1. **Event classification gap**: The structural-change detector only examined event *kind*, not path. `Modify(Data)` on any file — including config — was treated as non-structural.

2. **Unchanged-diff branch was no-op**: Driver skipped rebuild when `diff_discovered_package_paths` returned `Unchanged`, even when a structural change was pending. Config-only edits (same packages, new config values) fell into this gap.

3. **Override value freeze**: Rebuild passed the *resolved effective* `max_weight` back into rebuild, freezing the value. The correct pattern is to preserve the *original* CLI/env override intent (`Option<u32>`) and compute `override.unwrap_or(config.concurrency.max_weight)` — matching one-shot run semantics.

## Solution

### Part 1: Flag config-file edits as structural in watcher

Added path-based check in `contains_structural_change` (`watcher.rs:271-274`):

```rust
// After ignore filtering, check for config file
if path.file_name()
    .map(|name| name.to_string_lossy().starts_with("luchta-config."))
    .unwrap_or(false)
{
    return true;
}
```

This mirrors the inline check at `config.rs:110`. Deliberately no shared constant — config discovery and watcher detection have different scopes (root-only vs filename-anywhere), and inlining avoids false coupling.

### Part 2: Force rebuild on config-triggered structural signal even when package set unchanged

Created helper `rebuild_and_reconcile_watch_state` (`driver.rs:82-101`) used for both `Changed` and `Unchanged` branches:

```rust
async fn rebuild_and_reconcile_watch_state(
    context: &WatchIterationContext<'_>,
    package_paths: &BTreeSet<PathBuf>,
) -> Result<WatchControl> {
    if let Err(error) = context.session.rebuild_for_packages(package_paths).await {
        warn!(error = %error, "structural workspace rebuild failed; keeping previous graph");
        return Ok(WatchControl::Continue);
    }
    if let Err(error) = context.watcher_handle.reconcile_watch_roots().await {
        warn!(error = %error, "watch root reconcile failed after rebuild");
        return Ok(WatchControl::Continue);
    }
    Ok(WatchControl::Stop)  // Sentinel: success, caller should proceed
}
```

On error: warn + `Continue` (keeps previous graph, iteration restarts). On success: `Stop` (signals caller to proceed with normal flow). The sentinel semantics are inverted but safe — caller uses `matches!(result, Continue)` for early-return.

### Part 3: Preserve original CLI override across reloads

Modified `WatchSession` (`session.rs`) to store override intent, not resolved value:

```rust
pub(crate) struct WatchSession {
    // ... existing fields ...
    max_weight_override: Option<u32>,  // Original CLI/env override, not resolved value
}

// In rebuild_for_packages:
let max_weight = self.max_weight_override
    .unwrap_or(config.concurrency.max_weight);
```

`ReusedContextParams.max_weight_override` changed from `u32` to `Option<u32>`. The computation matches `run.rs` one-shot semantics: explicit override wins, config value applies when no override.

## Why This Works

1. **Path + event kind both considered**: Config-file `Modify(Data)` now enters structural path; non-config files fall back to existing event-kind logic.

2. **Unchanged triggers rebuild when structural pending**: Config-only edits that don't change package enumeration still force graph rebuild, ensuring new config values take effect.

3. **CLI override semantics preserved**: Storing `Option<u32>` (original intent) instead of `u32` (resolved value) means config reloads can update effective values when no CLI override exists, while CLI overrides remain dominant across rebuilds.

4. **Graceful degradation reused**: Invalid config emits warning, keeps previous graph active, recovers on next valid edit. Long-running workers survive via `WorkerManager` Arc preservation.

5. **CancellationToken already in place**: In-flight builds cancelled by existing drain-task logic before structural rebuild proceeds.

## Prevention Strategies

### Test Cases

- **Watcher unit test for config-file Modify(Data)**: `collect_watch_batch_sets_structural_for_root_config_data_edit` verifies `Modify(Data)` on `luchta-config.sh` => `structural: true`, while same event on `src/foo.rs` => `false`. Critical: E2E tests that inject `WatchBatch{structural:true}` directly bypass the real detector — pair with this unit test.

- **Config edit triggers rebuild**: E2E sends structural batch with config path changed, waits for `max_weight` value to update, asserts no unnecessary task rerun.

- **Malformed config recovery**: E2E corrupts config, asserts warning logged + previous graph active, then provides valid config and asserts recovery.

- **CLI override survives reload**: E2E starts watch with `--max-weight 5`, edits config to `maxWeight: 8`, asserts session `max_weight` stays at 5. **Use `stays_for(500ms, || current_max_weight() == 5)` — not `wait_until` — to ensure value remains dominant after reload batch processed.**

Files:
- `crates/luchta-cli/src/watch/watcher.rs` (unit tests)
- `crates/luchta-cli/src/watch/driver_e2e_tests.rs` (E2E tests)
- `crates/luchta-cli/src/watch/driver_e2e_support.rs` (harness helpers)

### Best Practices

- **Store original override intent, not resolved values** when hot-reloading config with CLI precedence. `Option<u32>` captures "no override provided" vs "explicit override X", computing effective value at rebuild time.

- **Pair injected E2E tests with detector unit tests** — E2E harness often bypasses real event classification, leaving detector logic untested.

- **Use `stays_for` assertions for value dominance checks** — `wait_until` only verifies a condition becomes true; `stays_for` proves it remains true after state changes propagate.

- **Reuse existing ArcSwap infrastructure** — WorkerManager Arc preservation, graceful degradation, warning+continue pattern already validated in prior structural-rebuild work.

### Code Review Checklist

- [ ] Config-file structural detection covers all extension variants (`.sh`, `.mts`, `.js`, etc.)?
- [ ] Unchanged-diff branch calls rebuild when structural pending?
- [ ] CLI override stored as `Option<T>`, never resolved effective value?
- [ ] Override precedence matches one-shot run semantics?
- [ ] Error paths return `Continue` (keep previous) vs success returning proceed sentinel?
- [ ] Unit test exercises real watcher detector, not injected batch?
- [ ] Override-precedence test uses `stays_for`, not instant `wait_until`?

## Related Issues

- **Prior Art:** [logic-errors/watch-structural-change-live-graph-rebuild-2026-07-02.md](./watch-structural-change-live-graph-rebuild-2026-07-02.md) — ArcSwap hot-swap, WorkerManager Arc preservation, graceful degradation pattern
- **Prior Art:** [logic-errors/lockfile-invalidation-selective-rebuild-2026-07-02.md](./lockfile-invalidation-selective-rebuild-2026-07-02.md) — Precedent for root-file structural detection
- **Prior Art:** [integration-issues/executable-config-loader-hardening-2026-06-08.md](../integration-issues/executable-config-loader-hardening-2026-06-08.md) — Config loader ETXTBSY retry + timeout (already hardened, no loader changes required)
- **GitHub:** [dobesv/luchta#172](https://github.com/dobesv/luchta/issues/172) — Config reload request
- **Plan:** `luchta-172-watch-config-reload`
