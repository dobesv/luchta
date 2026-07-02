---
title: "Live graph rebuild on structural workspace changes with preserved worker pool"
date: 2026-07-02
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: "Watch mode discovered workspace once at session start and never re-ran discovery when packages were added/removed/renamed/moved"
resolution_type: code_fix
severity: medium
tags:
  - watch-mode
  - structural-change
  - ArcSwap
  - package-graph
  - worker-pool
  - toctou
  - notify
plan_ref: luchta-167-watch-structural-changes
issue: "#167"
---

## Problem

Watch mode discovered the workspace once at `WatchSession::new` and never re-ran discovery. Adding/removing/renaming/moving packages was ignored until restart. Users had to manually restart watch mode to pick up new packages or remove deleted ones.

## Symptoms

- New packages added during watch session were invisible to the build loop
- Removed packages continued to appear in task graph (stale state)
- Renamed/moved packages triggered build failures referencing old paths
- No error surfaced — silently ignored structural changes

## Investigation Steps

Started from GitHub issue #167 requesting watch-mode structural change detection. Reviewed existing watch architecture in `driver.rs`, `session.rs`, `watcher.rs`. Found discovery called only once in `prepare_session_context` (`run.rs`). Traced event flow: `notify` → debouncer → bridge → driver batch channel → change handling.

Key discovery: `RunContext` held immutably in `WatchSession`. WorkerManager lifecycle (Arc, permanent shutdown flag) meant any context replacement had to preserve Arc identity.

Reviewed event classification: original `collect_changed_paths` discarded `EventKind`, flattening to `HashSet<PathBuf>`. Structural events (dir create/delete/rename) were indistinguishable from file edits.

## Root Cause

Two architectural gaps:

1. **Event classification loss**: Watch bridge discarded `EventKind` early (line ~191-200 in original `watcher.rs`), making folder create/delete indistinguishable from file edits.

2. **Immutable RunContext**: `WatchSession` held plain `RunContext` with no swap mechanism. WorkerManager's `is_shutdown` flag is permanent once set — creating a new context with a fresh manager would break the session.

## Solution

### 1. Preserve event kind + emit structural signal

Extended watch payload from `HashSet<PathBuf>` to `WatchBatch { changed_paths: HashSet<PathBuf>, structural: bool }`. Bridge classification helper `contains_structural_change` flags directory create/delete/rename events after ignore filtering.

File: `crates/luchta-cli/src/watch/watcher.rs`

### 2. Re-glob + package-set diff (not event replay)

On structural signal, perform real FS walk via `YarnWorkspace::discover` and diff `BTreeSet<PathBuf>` package paths against current set. This closes the atomic-creation TOCTOU: `mv` or `git checkout` materializing a nested `package.json` may not emit the nested file event, but re-glob catches the new package.

```rust
pub enum StructuralPackageSetDiff {
    Changed(BTreeSet<PathBuf>),  // rebuild needed
    Unchanged,                    // skip rebuild
    KeepPrevious,                 // discovery failed, keep existing graph
}
```

Error handling: malformed manifest → warn + `KeepPrevious` + continue. Session stays alive, retry on next change.

File: `crates/luchta-cli/src/watch/driver.rs`

### 3. ArcSwap<RunContext> + WorkerManager transfer

Changed `WatchSession` to hold `ArcSwap<RunContext>` for lock-free reads with atomic swap.

```rust
pub struct WatchSession {
    run: ArcSwap<RunContext>,
}
```

`rebuild_for_packages` reconstructs `PackageGraph` + `TaskGraph` for new package set while **transferring** existing `Arc<WorkerManager>`:

```rust
let worker_manager = Arc::clone(&current.worker_manager);
// ... build new graph ...
self.run.store(Arc::new(RunContext { ..., worker_manager }));
```

Atomicity: all fallible steps (discovery, graph build, config load) succeed before `store()`. If any fail, previous context remains intact (graceful degradation).

Test: `watch_session_rebuild_for_packages_swaps_graphs_and_keeps_worker_manager` asserts `Arc::ptr_eq` on manager before/after.

File: `crates/luchta-cli/src/watch/session.rs`

### 4. Registry hygiene

`TaskWatchRegistry` (`Arc<Mutex<HashMap<TaskId, _>>>`) is pruned in place to preserve Arc identity:

```rust
let task_watch_registry = Arc::clone(&current.task_watch_registry);
retain_task_watch_registry_task_ids(&task_watch_registry, &live_task_ids);
```

Removed-package task entries drop immediately; new tasks register on next cycle.

File: `crates/luchta-cli/src/watch/registry.rs`

### 5. Structural latch + iteration control

`PendingChanges` holds `PendingState { paths: HashSet<PathBuf>, structural: bool }` under single Mutex. `mark_structural()` sets latch and returns whether state was empty (wakes loop). `take_structural()` clears/consumes. Repeated structural batches coalesce into one rebuild.

Iteration order: `take_structural()` → diff → rebuild if `Changed` → reconcile watcher roots → drain paths → run cycle.

File: `crates/luchta-cli/src/watch/driver.rs`

### 6. Watch-dir reconciliation (gotcha fixed)

Reconcile must use **full `discover_watch_dirs` walk** (all content dirs: `src/`, `tests/`, etc.), NOT just structural roots+ancestors. Otherwise content-dir watches get orphaned and ordinary source edits stop being detected.

```rust
let desired_dirs = discover_watch_dirs(workspace_root, &self.ignore_filter)?;
self.reconcile_watched_dirs(desired_dirs)  // adds new, unwatches orphans
```

File: `crates/luchta-cli/src/watch/watcher.rs`

## Why This Works

1. **ArcSwap enables atomic context swap** without global locks. Readers always see consistent graph state.

2. **Re-glob closes TOCTOU**: Package-set diff against real FS, not event replay. Atomic `mv`/`git checkout` caught by full walk even if nested file events are missed.

3. **WorkerManager Arc identity preserved**: Workers stay alive across rebuilds, no restart cost.

4. **Graceful degradation**: Rebuild failure leaves previous context intact; malformed manifests don't crash session.

5. **Full-tree reconcile preserves content watches**: Ordinary edits still detected after structural changes.

## Applicable When

- Tool discovers packages/dirs at startup and needs live updates
- Worker pool must survive graph rebuilds (stateful workers, warm cache)
- Structural changes may be atomic (mv, git operations) and miss nested events
- Graceful degradation preferred over crash on transient errors

## Prevention Strategies

### Test Cases

- **Structural E2E**: Add/remove/rename/move package → graph updates → tasks run at new location
- **Worker identity**: `Arc::ptr_eq` on manager before/after rebuild
- **Malformed manifest**: Corrupt `package.json` → warn, keep previous graph, no crash
- **Mid-build structural**: Change during build → cancel → rebuild → settle without deadlock
- **Reconcile preserve**: Remove package → its sibling `src/` still watched → edit detected
- **Latch coalescing**: Rapid structural signals → single rebuild

File: `crates/luchta-cli/src/watch/driver_e2e_tests.rs`

### Best Practices

- **Wrap mutable shared state in ArcSwap for read-heavy paths** — lock-free reads + atomic swap
- **Transfer Arc identity, not ownership** — clone the Arc before rebuild, store clone in new context
- **Do fallible work before swap** — error leaves previous context intact
- **Use real FS walk for structural detection, not event replay** — closes TOCTOU for atomic ops
- **Reconcile against full discovery walk, not structural roots** — preserve content watches
- **Prune registry in place to preserve Arc identity** — concurrent holders see same registry

### Code Review Checklist

- [ ] `ArcSwap` used for hot-path immutable reads?
- [ ] `Arc::clone` extracted before fallible rebuild steps?
- [ ] All fallible graph/config work done before `store()`?
- [ ] Structural detection uses re-glob, not event replay?
- [ ] Package-set diff gates rebuild (no false positives)?
- [ ] Reconcile uses full `discover_watch_dirs`, not ancestor-only?
- [ ] Registry pruned in place (`Arc::clone` + `retain`)?
- [ ] Error paths warn + continue, never crash session?

## Related Issues

- **Prior Art:** [logic-errors/watch-mode-session-run-split-and-no-lost-changes-2026-06-30.md](./watch-mode-session-run-split-and-no-lost-changes-2026-06-30.md) — watch loop, ActiveCycle cancellation, PendingChanges latch architecture
- **Prior Art:** [logic-errors/watch-input-aware-rebuild-registry-2026-07-01.md](./watch-input-aware-rebuild-registry-2026-07-01.md) — TaskWatchRegistry, input fingerprinting, dirty detection
- **GitHub:** [dobesv/luchta#167](https://github.com/dobesv/luchta/issues/167) — Structural change detection request
- **Plan:** `luchta-167-watch-structural-changes`

## Appendix: CodeScene Test-Module Extraction

During final quality gates, large `#[cfg(test)]` modules inflated the HOST file's per-file cohesion/duplication health. Repo convention: extract test modules AND their helper/harness into sibling `#[path = "..._tests.rs"]` files so production files stay green.

Example pattern from `run.rs`:
```rust
#[cfg(test)]
#[path = "run_watch_tests.rs"]
mod watch_tests;
```

Also: bundle 5+ fn args into params structs; collapse near-identical test poll/discover helpers via generic combinators + shared arrange/act helper.

This was the single biggest time-sink of the task — test refactoring triggered repeated CodeScene delta checks. Document the pattern so future work doesn't thrash.
