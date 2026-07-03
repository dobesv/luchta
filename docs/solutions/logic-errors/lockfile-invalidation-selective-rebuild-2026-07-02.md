---
title: "Lockfile change triggers selective rebuild for affected packages only"
date: 2026-07-02
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: "Watch mode received yarn.lock modify events but dirty_packages_for_changes mapped root lockfile to no package → returned empty → watch reported up-to-date with no re-runs"
resolution_type: code_fix
severity: high
tags:
  - watch-mode
  - lockfile
  - yarn
  - invalidation
  - dependency-tracking
  - cache-consistency
  - canonicalization
  - path-matching
plan_ref: luchta-169-watch-lockfile-invalidation
issue: "#169"
---

## Problem

Watch mode did not re-run tasks when `yarn.lock` changed, even when resolved dependency versions changed. The watcher received the lockfile modify event, but `dirty_packages_for_changes` only mapped changed paths to packages for known task inputs or paths inside package directories. Root `yarn.lock` is neither → returned empty set → watch reported "up-to-date" with no re-runs.

## Symptoms

- Running `yarn install` or `yarn upgrade` during watch mode caused no rebuilds
- Tasks whose dependency versions changed continued using stale cached outputs
- No error or warning appeared; watch simply remained idle after lockfile change
- Packages whose declared dependencies changed were indistinguishable from unaffected packages

## Investigation Steps

1. **Verified watcher receives lockfile events**: Added debug logging to `spawn_watcher`. Confirmed `notify-debouncer-full` emits modify events for workspace root (including `yarn.lock`).

2. **Traced change→package mapping**: `dirty_packages_for_changes` (`registry.rs:130`) only maps paths to packages if path is a declared task input or resides inside the package directory. Root `yarn.lock` fails both checks.

3. **Analyzed cache invalidation path**: Cache's `PkgDepMismatch` (`decide.rs`) only fires during task *evaluation*, never triggers *selection*. A lockfile-only change never reaches evaluation → `PkgDepMismatch` unreachable.

4. **Verified cache dependency hash mechanism**: `gather_pkg_dep_pairs` (`cache_ctx.rs:133`) already computes per-package (dep_name, resolved_version) pairs for `pkg_dep_hash`. This is the authoritative source for what the cache considers a "dependency change".

5. **Clarified transitive boundary**: Plan assumed `Lockfile::all_dependencies` returned resolved versions transitively. Verified in `yarn1.rs` / `yarn_berry.rs`: `all_dependencies(key)` returns only **direct children as specifier strings** (e.g., `^3.0.0`), NOT resolved versions, NOT recursive. A deep transitive resolved-version bump is NOT detected — and this matches the cache's behavior (correct-by-design).

6. **Path canonicalization pitfall**: Review identified that `spawn_watcher` canonicalizes `workspace_root` before emitting paths, but `LockfileWatchState::new` must also canonicalize before joining. Raw `workspace_root.join("yarn.lock")` from relative invocation like `luchta watch .` would fail to match watcher events.

## Root Cause

Root `yarn.lock` had no mapping to packages in watch invalidation logic. The watcher received events but the change-to-package mapper discarded them because the lockfile path doesn't belong to any single package's input set.

Additionally, the cache's dependency-change detection (`PkgDepMismatch`) only triggers for tasks already selected for evaluation. Lockfile-only changes never entered evaluation, so dependency mismatches were never discovered.

## Solution

### New `LockfileWatchState` Module

Created `crates/luchta-cli/src/watch/lockfile_watch.rs` with a per-package baseline of resolved dependency pairs:

```rust
pub(crate) struct LockfileWatchState {
    lockfile_path: PathBuf,  // canonicalized workspace_root.join("yarn.lock")
    baseline: HashMap<PackageName, Vec<(String, String)>>,  // (dep_name, resolved_version)
}
```

**Key methods**:

- `new(workspace_root)`: Canonicalizes workspace root before joining `yarn.lock` — critical for matching watcher events.
- `rebuild_baseline(...)`: Clears baseline, repopulates via `gather_pkg_dep_pairs` for each package on valid lockfile parse.
- `affected_packages(...)`: Diffs current lockfile state against baseline; returns packages with changed dep pairs. Conservative fallback: lockfile `Failed`/`Absent` → all packages affected.

**Conservative fallback**: If lockfile fails to parse or disappears, treat ALL packages as affected. Safe over correct.

### Driver Integration

Modified `run_one_iteration` in `driver.rs`:

1. **Before the empty check**: Union lockfile-affected packages into `affected` set.
2. **After computing affected**: Refresh the baseline for next iteration.

```rust
let mut affected = dirty_packages_for_changes(...);
let lockfile_changed = changed.iter().any(|path| path == &lockfile_path);
if lockfile_changed {
    affected.extend(lockfile_state.affected_packages(...));
    lockfile_state.rebuild_baseline(...);
}
if affected.is_empty() {
    context.ui.up_to_date();
    return Ok(WatchControl::Continue);
}
```

**Lock scope**: `Arc<Mutex<LockfileWatchState>>` held briefly for `lockfile_path()` check and for the state mutation. No async await while locked.

### Canonicalization for Path Matching

`LockfileWatchState::new` canonicalizes workspace root:

```rust
let canonical_root = workspace_root
    .canonicalize()
    .unwrap_or_else(|_| workspace_root.to_path_buf());
Self {
    lockfile_path: canonical_root.join("yarn.lock"),
    ...
}
```

This ensures `lockfile_path` matches the canonical paths emitted by `spawn_watcher` (which canonicalizes at line 144 of `watcher.rs`). The driver comparison is a single canonical-equality check.

### Reuse of Existing `gather_pkg_dep_pairs`

Intentionally reused `gather_pkg_dep_pairs` from `cache_ctx.rs` rather than implementing custom lockfile diffing. This guarantees watch-mode invalidation fires exactly when the cache's `pkg_dep_hash` would change. No drift from cache correctness.

## Why This Works

1. **Cache consistency by design**: Reusing `gather_pkg_dep_pairs` ties watch invalidation to the same dependency-pair computation the cache uses for `pkg_dep_hash`. If cache would see a mismatch, watch will re-run; if cache wouldn't, watch won't.

2. **Selective invalidation**: Only packages whose declared dependency versions changed are marked affected. Package A can rerun while Package B stays idle.

3. **Conservative fallback**: If lockfile parse fails or lockfile is deleted, ALL packages are affected. Safe over correct — ensures we rebuild rather than using stale data.

4. **Path matching works regardless of invocation**: Canonicalization in both `spawn_watcher` and `LockfileWatchState::new` ensures `luchta watch .`, `luchta watch $(pwd)`, and `luchta watch /full/path` all produce matching canonical paths.

5. **Transitive clarity**: Deep transitive resolved-version bumps are NOT detected — and this is correct. `Lockfile::all_dependencies` returns direct-child specifier strings only, matching what the cache tracks. This prevents false positives.

## Prevention Strategies

### Test Cases

- `parsed_lockfile_bump_affects_only_changed_package`: Multi-package workspace; bump dep for package A only → A affected, B not.
- `deep_transitive_resolved_version_change_is_not_detected_matches_cache_dep_hash`: Only child of a declared dep changes resolved version → no package affected (matches cache behavior).
- `absent_lockfile_affects_all_packages`: Remove lockfile → all packages affected.
- `new_canonicalizes_non_canonical_workspace_root_for_lockfile_path`: Verify canonicalization for path matching.

### Best Practices

- **Always canonicalize paths for watcher comparisons**. Watcher emits canonical paths; any comparison must match.
- **Reuse cache logic for invalidation**. Never reimplement dependency diffing separately — it will drift from cache correctness.
- **Union before the empty check**. Lockfile-affected packages must be added to `affected` BEFORE the `is_empty()` gate, not after.
- **Lock briefly, no await**. `Arc<Mutex<LockfileWatchState>>` should be held for in-memory operations only; release before any async operations.
- **Conservative fallback for parse failures**. If lockfile goes sideways, rebuild everything rather than risking stale dependency data.

### Code Review Checklist

- [ ] Does `LockfileWatchState::new` canonicalize workspace_root before joining?
- [ ] Is lockfile-affected package union performed BEFORE the `affected.is_empty()` check?
- [ ] Is baseline refresh performed AFTER affected computation?
- [ ] Is `gather_pkg_dep_pairs` reused (not reimplemented)?
- [ ] Does the driver handle `Failed`/`Absent` lockfile states conservatively?
- [ ] Is the `Arc<Mutex<LockfileWatchState>>` lock scope brief with no await inside?

## Related Issues

- **GitHub:** [dobesv/luchta#169](https://github.com/dobesv/luchta/issues/169)
- **Prior Art:** [logic-errors/watch-input-aware-rebuild-registry-2026-07-01.md](./watch-input-aware-rebuild-registry-2026-07-01.md) — Input-aware watch registry (output exclusion, content verification).
- **Prior Art:** [logic-errors/watch-mode-session-run-split-and-no-lost-changes-2026-06-30.md](./watch-mode-session-run-split-and-no-lost-changes-2026-06-30.md) — WatchSession/ActiveCycle split; lifecycle for cross-iteration state.
- **Prior Art:** [performance-issues/yarn-lock-once-per-run-enum-state-2026-06-15.md](../performance-issues/yarn-lock-once-per-run-enum-state-2026-06-15.md) — `LockfileState` enum to avoid redundant lockfile reads; reused here.
