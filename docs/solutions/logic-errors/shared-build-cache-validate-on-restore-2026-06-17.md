---
title: "Shared build cache: coarse lookup key + validate-on-restore correctness pattern"
date: 2026-06-17
category: logic-errors
problem_type: logic_error
component: luchta-cache/shared, luchta-cli/run/dispatch
root_cause: "Coarse lookup key omitting file content hashes caused stale restores; input-only validation predicate for restore different from local-cache skip decision"
resolution_type: code_fix
severity: critical
tags:
  - cache-correctness
  - validate-on-restore
  - content-validation
  - flock-inode
  - shared-cache
  - cross-worktree
  - two-phase-cache
plan_ref: shared-build-cache
---

## Problem

Shared build cache used a coarse `input_key` that omitted resolved input file content hashes, silently restoring stale outputs when input file contents changed (pattern unchanged → identical key). Additionally, restore validation incorrectly reused local cache's `decide()` which required current-tree outputs to match — impossible when outputs don't exist yet (shared restore provides them from blobs). Lock file deletion in GC broke `flock` mutual exclusion.

## Symptoms

```text
# Stale restore: input content changed but pattern unchanged → identical input_key
Task at commit A: input "src.txt" content "v1" → cached
Edit "src.txt" to "v2" at commit C → input_key UNCHANGED → shared cache restored stale "v1" output

# Full decide() rejected legitimate restores
Cross-worktree restore: empty worktree has no outputs → decide() returns Run → shared hit rejected

# GC flock race: deleted lock sidecars
GC deleted *.bincode.lock while writer held flock → later writer created same path, different inode → mutual exclusion bypassed
```

Test `cross_commit_shared_cache_hit` exposed both: unchanged input restore worked, but changed-input case got stale output.

## Investigation Steps

1. Traced `input_key` derivation in `derive_input_key(task_spec_hash, env_hash, pkg_dep_hash, combined_dep_outputs_hash)` — none include resolved input FILE CONTENT hashes.

2. Compared with LOCAL cache's two-phase model: `cacheable_prior()` (spec/env/pkg/dep hashes) BEFORE `patterns_unchanged()` (re-resolves patterns, compares FileEntry content hashes).

3. Attempted fix: fold resolved-input-content hash into `input_key` — **rejected**: effective inputs (post worker-refinement) are unknowable before running the task. Cannot compute what files a task will actually read until it runs.

4. Daedalus ruling: `input_key` stays unchanged as COARSE CANDIDATE SELECTOR. SAFETY BOUNDARY = validation on hit, mirroring local cache's proven two-phase model.

5. Traced `decide()` internals: `cacheable_prior()` + `patterns_unchanged(&prior.inputs, ...)` + `patterns_unchanged(&prior.outputs, ...)`. Output validation FAILS when outputs absent — exactly the shared-restore case.

6. Created `decide_shared_restore()` that validates inputs ONLY, skipping output-presence check. Output integrity is inherent: blob is content-addressed by `outputs_hash`.

7. GC investigation: `fs2`/`flock` guards INODE, not pathname. Deleting `<commit>.bincode.lock` while writer holds lock → later writer creates same path → different inode → two writers both "hold" lock → overlapping RMW → lost entries.

## Root Cause

**Stale-restore bug**: Coarse lookup key without content-validation step. `input_key` deliberately omits file content hashes because effective inputs are unknowable pre-run. Cache lookup key that omits content MUST gate on content-validation before restoring. Local cache's `decide()` does this correctly; initial shared implementation skipped it.

**Restore rejection bug**: `decide()` validates outputs exist/match in current tree — wrong for restore case where outputs are being provided FROM the cache. Restore validation must check inputs only.

**GC flock race**: Advisory file locks are inode-scoped. Deleting an in-use lock file allows pathname reuse with different inode, breaking mutual exclusion.

## Solution

### Fix 1: Validate-on-restore with staging

Modified shared restore path to stage candidates before validation:

```rust
// crates/luchta-cache/src/shared/blob.rs
pub struct StagedRestore {
    staging_dir: TempDir,
    package_dir: PathBuf,
}

impl StagedRestore {
    pub fn commit(self) -> io::Result<()> {
        // Move staged files into package tree only after validation passes
        fs::rename(&self.staging_dir, &self.package_dir)
    }
    
    pub fn discard(self) {
        // TempDir drops, cleaning staging without polluting package tree
    }
}
```

```rust
// crates/luchta-cache/src/shared/mod.rs
pub struct StagedCandidate {
    pub record: TaskRunRecord,
    pub staged: StagedRestore,
}
```

Restore flow:
1. Load candidate snapshot from merged index (O(1) in-memory lookup)
2. Extract blob to staging dir (doesn't mutate package tree yet)
3. Validate candidate via `decide_shared_restore()` — if true, `commit()`; else `discard()` and try next candidate

### Fix 2: Input-only validation for shared restore

```rust
// crates/luchta-cache/src/decide.rs
pub fn decide_shared_restore(record: &TaskRunRecord, current: &CurrentState<'_>) -> bool {
    // Check: prior task succeeded + spec/env/pkg/dep hashes match
    if !cacheable_prior(record, current) {
        return false;
    }
    
    // Check: input patterns unchanged (FileEntry content hashes match)
    // Does NOT check output presence — outputs come from staged blob
    patterns_unchanged(&record.inputs, current.resolver, current.tree)
}
```

Key distinction from `decide()`:
- **Local skip**: validate inputs AND outputs present/match → Skip
- **Shared restore**: validate inputs ONLY → restore outputs from blob

Output integrity is guaranteed: blob is content-addressed by `outputs_hash`.

### Fix 3: Never delete flock sidecars in GC

```rust
// crates/luchta-cache/src/shared/gc.rs
pub fn garbage_collect(paths: &SharedCachePaths, retention: Duration) {
    // Delete snapshot files older than retention
    for entry in fs::read_dir(&paths.snapshots_dir)? {
        let path = entry?.path();
        if path.extension() == Some("bincode") && age(&path) > retention {
            fs::remove_file(&path)?;
        }
        // NEVER delete .bincode.lock sidecars
        // flock guards inode, not pathname — deletion allows inode reuse race
    }
    
    // Delete blobs older than retention
    for entry in fs::read_dir(&paths.blobs_dir)? {
        let path = entry?.path();
        if path.extension() == Some("zst") && age(&path) > retention {
            fs::remove_file(&path)?;
        }
    }
}
```

Leaking tiny empty lock files is vastly safer than breaking flock mutual exclusion.

## Why This Works

**Validate-on-restore**: Coarse lookup key (spec/env/pkg/dep hashes) widens candidate pool. Content validation (`patterns_unchanged` comparing FileEntry hashes) gates the actual restore. This mirrors the local cache's proven two-phase model while respecting that effective inputs are unknowable pre-run.

**Input-only validation**: Shared restore PROVIDES outputs from the blob — requiring current-tree outputs to exist/match would reject every legitimate restore into empty worktree. Output integrity is inherent because the blob is content-addressed by the stored `outputs_hash`.

**Flock sidecar safety**: `fs2`/`flock` guards the inode. Deleting a lock file that any process may hold/await allows same-path-different-inode reuse, bypassing mutual exclusion. Pathname-based lock lifecycle management is a concurrency footgun; lock files must remain until no process can possibly hold them.

## Prevention Strategies

**Test Cases:**
- Cross-commit cache hit with unchanged inputs → hit, no re-execution
- Cross-commit cache hit with changed input content → miss, re-execute
- Cross-worktree restore into empty tree → hit, restore succeeds
- Input-only validation: outputs absent, input hashes match → returns true
- Input content differs: restore validation returns false
- GC preserves `.bincode.lock` files while deleting aged snapshots
- Concurrent snapshot merges serialized by flock

**Best Practices:**
- Cache lookup key that omits content hashes MUST gate on content-validation before restore
- Restore validation has different predicate than local-cache skip validation
- Never unlink a lock file that any process may hold/await
- Test shared-cache correctness under concurrent builds (spawn_blocking for blocking I/O)
- Hydrate local cache after shared restore: downstream invalidation needs correct outputs_hash

**Code Review Checklist:**
- [ ] Does shared-restore validation check inputs ONLY, not outputs?
- [ ] Is content validation performed on candidate hit before restoring?
- [ ] Does GC preserve flock sidecar lock files?
- [ ] Are blob writes atomic (temp+sync+rename)?
- [ ] Are readers read-tolerant (missing/corrupt → miss, not error)?

## Related Issues

- **Jira:** [dobesv/luchta#22](https://github.com/dobesv/luchta/issues/22) — Shared build cache feature
- **Related Solution:** [logic-errors/env-control-cache-correctness-single-resolver-2026-06-16.md](../logic-errors/env-control-cache-correctness-single-resolver-2026-06-16.md) — Hash boundaries and single-source-of-truth for env resolution
- **Related Solution:** [logic-errors/uncached-task-detected-output-coupling-2026-06-12.md](../logic-errors/uncached-task-detected-output-coupling-2026-06-12.md) — Effective outputs for cache invalidation
- **Related Solution:** [security-issues/cross-package-input-expansion-security-2026-06-16.md](../security-issues/cross-package-input-expansion-security-2026-06-16.md) — Path-escape hard-fail and output scope guard

## Additional Learnings

**Lazy load-once in-memory index:** Per-build `OnceLock` merged index over window of last N branch commits (newest-wins), built exactly once on first restore, avoids O(tasks × candidate-files) disk/deserialize blowup. Retain loaded snapshots in memory for blob-miss fallback.

**Dirty/clean read-window decision:** Candidate commit window reads BOTH `<commit>` and `<commit>-dirty` snapshots. For LOCAL shared-filesystem use case, dirty work trees are the norm; content-keyed validation makes any hit safe. Write-side keeps dirty entries in `-dirty` files (never pollutes clean key). Clean-only reads are a future CI/remote concern.

**Test isolation gotcha:** Test using GLOBAL atomic counter to prove "load once" was flaky under shared-process `cargo test` (passed only under nextest's process isolation). Fix: per-instance injected counter (no global mutable test state). Env-dependent helpers made pure (take env value as param) for parallel-safe tests.

**Non-fatal snapshot lock failures:** `SnapshotStore::merge_entry` degrades gracefully on lock open/acquire failure. Instead of panic, logs warning, returns `MergeResult::SkippedLockUnavailable`. Shared snapshot metadata write is best-effort; local cache/write path remains successful.
