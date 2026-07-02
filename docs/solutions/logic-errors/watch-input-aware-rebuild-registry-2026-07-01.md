---
title: "Input-aware watch invalidation to break the shared-cache rebuild loop"
date: 2026-07-01
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: "Watch mode treated ANY non-gitignored change as a rebuild trigger and mapped it to a package by directory prefix; shared-cache restore writes a package's OUTPUT files back into the package dir, which the watcher saw as source changes, causing an infinite rebuild loop"
resolution_type: code_fix
severity: high
tags:
  - watch-mode
  - rebuild-loop
  - shared-cache
  - input-fingerprint
  - output-globs
  - blake3
  - inotify
plan_ref: luchta-watch-fixes
issue: "#161"
---

## Problem

`luchta watch build` rebuilt the same packages over and over, forever. It looped
even on no-op cycles that reported `✔ 0 ⏩ 0` (nothing built, nothing written by
the tasks themselves), so the trigger was clearly not a task producing output.

Two related issues were fixed in the same change:
- **#161** watch mode keeps building over and over.
- **#160** watch mode blanked the terminal on every rebuild (destroying scrollback).

## Symptoms

```text
[watch] rebuilding…
✔ 801 ⏩ 801 📥 1 ⌚ 11s ...
[watch] cancelled (new changes)
[watch] change detected: @formative/main, @formative/react-main
[watch] rebuilding…
✔ 0 ⏩ 0 ⌚ 0s ...          <- nothing built, yet a "new change" was pending
[watch] cancelled (new changes)
[watch] change detected: @formative/main, @formative/react-main
... loops forever, always the same two packages ...
```

Note the `📥 1` (shared-cache hit) on an early cycle — a strong hint that cache
restore was involved.

## Investigation Steps

1. Traced the watch pipeline: notify watcher → debounced batches → `PendingChanges`
   → drain → `affected_packages_from_paths` (deepest package-dir prefix match) →
   run cycle. The change→package mapping was purely path-prefix based.
2. Traced the shared-cache HIT restore path in `crates/luchta-cache/src/shared/blob.rs`:
   `restore_blob_with_meta` extracts a package's OUTPUT files into a
   `blob-restore-meta-*` tempdir created INSIDE the package dir, then
   `StagedRestore::commit()` → `move_non_meta_files` `fs::rename`s those output
   files into the package dir.
3. Realized the watcher observes those restored OUTPUT files as if they were
   source edits, maps them to the package (`@formative/main`) plus transitive
   dependents (`@formative/react-main`), and triggers another cycle → which
   restores again → infinite loop. It loops even on `✔ 0 ⏩ 0` cycles because the
   prior restore's file events are still flowing through the pending pipeline.

## Root Cause

**Category error:** the watcher treated *any* non-gitignored file change as a
rebuild trigger. A package's own build **outputs** are not inputs, so restoring
them from cache should never trigger a rebuild — but the path-prefix mapping
could not tell inputs from outputs.

## Failed / Superseded Approaches

Recorded so nobody repeats them:

1. **"Suppress-set" (ignore my own writes).** Thread the set of paths the restore
   wrote up to the watch loop and one-shot-filter them from the next drain.
   *Worked but was a hack:* it chased the symptom (self-writes) rather than the
   category error, left edge cases (staging-dir events bypassing the set,
   partial-restore path loss), and required plumbing a `Vec<PathBuf>` /
   `Arc<Mutex<HashSet<PathBuf>>>` through blob → dispatch → run → session → driver.

2. **On-disk input-glob gate.** On each drain, read `.luchta/cache`
   `TaskRunRecord.input_patterns` per task and keep only changed paths matching an
   input glob. *Better (it fixed outputs at the root), but* coupled the watcher to
   per-drain on-disk cache reads and reintroduced a bootstrapping question.

## Solution — in-memory, input-aware task registry

An in-memory per-task **input registry**, populated as tasks are dispatched. New
module `crates/luchta-cli/src/watch/registry.rs`.

```rust
struct InputFingerprint { mtime_ns: i128, size: u64, hash: [u8; 32] }
struct TaskWatchState {
    package: PackageName,
    package_dir: PathBuf,
    input_globset: GlobSet,        // from record.input_patterns
    output_globset: GlobSet,       // from record.output_patterns
    inputs: HashMap<PathBuf, InputFingerprint>, // absolute path -> fingerprint
}
type TaskWatchRegistry = Arc<Mutex<HashMap<TaskId, TaskWatchState>>>;
```

Registration happens on **all three** dispatch decisions, using data already in
`TaskRunRecord` (crates/luchta-cache/src/record.rs): `Decision::Skip` (from the
`prior` record already read), `Decision::Run` (from the freshly built record),
`Decision::SharedHit` (from `hit.record`). This means the registry is populated
even when every task is cached on the first cycle — no bootstrap gap.

A changed path dirties a task's package only when (`dirty_packages_for_changes`):
1. It matches the task's **output globs** → **never** (the root-cause fix:
   restored/produced outputs can't self-trigger, even if input globs overlap
   outputs).
2. It is a **known input** and its content actually changed: stat `(mtime, size)`
   fast-path; on mismatch re-hash with `blake3_file` and dirty only if the hash
   differs (filters touch-only / restored-but-identical events). The fingerprint
   is refreshed on every real change so the next distinct edit re-dirties.
3. It is a **new file** under the package that matches an **input glob**.
4. Fallback: a task that declares **no inputs at all** (empty input globset and no
   known inputs) conservatively dirties on any **non-output** change in its
   package dir — preserving responsiveness / no-lost-changes for such tasks.

Dirty tasks are grouped by package → that is the affected set for the next cycle.

## Why This Works

- **Outputs are excluded by definition.** The output-glob check runs first, so a
  cache-restored or freshly-produced output file can never trigger the task that
  produced it. This kills the loop at the source without a suppress-set.
- **"A task that hasn't run needs no re-run."** Registering from the record on
  Skip/SharedHit (not only Run) removes the bootstrapping problem entirely.
- **Match globs, not just resolved file lists**, so brand-new files (e.g.
  `src/new.ts` under `src/**/*.ts`) still trigger.
- **Content verification** (mtime/size → hash) eliminates the touch-no-change /
  identical-restore churn class — arguably the deepest cause of the loop.

## Key Implementation Notes / Gotchas

- **Do filesystem I/O off the lock.** `dirty_packages_for_changes` computes a
  `PathProbe` (stat + hash) for every changed path *before* taking the registry
  `Mutex`; the lock is held only for in-memory fingerprint comparison/update.
- **Distinguish deletion from transient errors.** `PathProbe { Present(fp),
  Deleted, Unknown }`: `Deleted` only for `io::ErrorKind::NotFound`; other read
  errors → `Unknown` → not dirty, state untouched (avoids spurious rebuilds on
  EAGAIN/permission hiccups).
- **mtime computation must match the cache** (`i128` ns since epoch, matching
  `FileEntry.mtime_ns`) so the fast-path comparison against stored fingerprints is
  valid.
- The watcher still watches package **directories** (notify, non-recursive) and
  filters events against the registry in the drain — no notify reconfiguration.
- Also kept `blob-restore-meta*` in the watcher ignore filter as belt-and-suspenders.

## #160 — stop blanking the screen

Watch mode printed `\x1Bc` (RIS, full terminal reset) at each cycle start, wiping
scrollback. Removed it (and the dead helpers/field/test); output now scrolls,
preserving prior build output and change history.

## Diagnostic — `--show-changed-files`

Added `luchta watch --show-changed-files` which, on each rebuild, lists the
changed files (first 10 + "… and N more"), relative to the repo root. Gated behind
the flag so default output is unchanged. Useful for diagnosing unexpected rebuilds.

## Prevention Strategies

### Test Cases (in `registry.rs`)
- unchanged input → not dirty; changed content → dirty; touch-only → not dirty.
- new file matching an input glob → dirty.
- output/staging path (not an input) → ignored (the #161 loop-break proof).
- undeclared-input task dirties on any non-output change but NOT on its own output.
- output globs take precedence over overlapping input globs.
- confirmed deletion of a known input → dirty; transient `Unknown` probe → not dirty.
- second distinct edit re-dirties (fingerprint refreshed).

### Best Practices
- **Watch invalidation must be input-aware.** Never treat "any file changed in a
  package dir" as a rebuild trigger when the tool knows each task's declared
  inputs/outputs.
- **Exclude a task's own outputs first**, before any input/fallback matching.
- **Prefer in-memory registration from run records over re-reading on-disk cache**
  in a hot path; register on cache hits too so there is no bootstrap gap.
- **Verify content (hash), not just mtime/size**, before declaring a change.
- **Keep filesystem I/O out of shared-lock critical sections.**

### Code Review Checklist
- [ ] Do a task's own outputs (output globs) get excluded before input matching?
- [ ] Are new files (glob match), not just previously-resolved files, handled?
- [ ] Is content-hash verification used to suppress touch-only events?
- [ ] Is stat/hash done outside the registry lock?
- [ ] Are transient I/O errors distinguished from real deletions?
- [ ] Is the registry populated on Skip/SharedHit, not only Run?

## Related Issues

- **Prior Art:** [logic-errors/watch-mode-session-run-split-and-no-lost-changes-2026-06-30.md](./watch-mode-session-run-split-and-no-lost-changes-2026-06-30.md) — watch loop, PendingChanges/ActiveCycle, no-lost-changes invariant (still holds; this change only replaces the change→package mapping).
- **Prior Art:** [logic-errors/shared-build-cache-validate-on-restore-2026-06-17.md](./shared-build-cache-validate-on-restore-2026-06-17.md) — shared-cache restore staging into the package dir.
- **Prior Art:** [logic-errors/since-filter-selection-and-gix-change-detection-2026-06-18.md](./since-filter-selection-and-gix-change-detection-2026-06-18.md) — `affected_packages_from_paths` (still used by `luchta run --since`, now bypassed by watch).
- **GitHub:** [dobesv/luchta#161](https://github.com/dobesv/luchta/issues/161), [dobesv/luchta#160](https://github.com/dobesv/luchta/issues/160)
- **Plan:** `luchta-watch-fixes`
