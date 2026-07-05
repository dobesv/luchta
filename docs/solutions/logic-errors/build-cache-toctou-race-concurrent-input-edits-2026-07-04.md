---
title: "Fix build-cache TOCTOU race against concurrent input edits"
date: 2026-07-04
category: "logic-errors"
problem_type: logic_error
component: "cache-layer"
root_cause: "input hash snapshot taken post-execution recorded wrong state"
resolution_type: code_fix
severity: high
tags:
  - race-condition
  - toctou
  - cache-correctness
  - concurrency
  - input-resolution
plan_ref: "concurrent-change-handling"
---
# Problem

Luchta's task cache resolved input file hashes AFTER task execution, storing them as the record's authoritative inputs. A concurrent edit during task execution (H1 → H2 while task reads H1) produced output from H1 but recorded metadata with H2 hashes — causing later wrongly-skipped rebuilds and stale shared-cache restores. Watch mode could also miss rebuilds.

# Symptoms

- Cache hit after input rollback: task output was from H1, cache metadata recorded H2, subsequent run skipped rebuild despite H1 ≠ H2
- Shared cache restore brought stale outputs: remote shard poisoned by bad metadata
- Watch mode missed rebuilds when inputs changed mid-run
- No explicit error; silent cache-correctness violation

# Investigation Steps

1. Traced cache-write path in `build_cache_write_context` — input resolution happened after task completion
2. Realized the gap: task reads file at T1, hashes resolved at T2, edit between T1/T2 corrupts metadata
3. Drafted fix: capture pre-execution snapshot, compare post-run; reject cache write on mismatch
4. Early attempt marked worker-detected runs "uncacheable" — broke legitimate cache/skip lifecycle (e2e test `cache_worker_detected_prefixed_inputs_rerun_on_root_and_upstream_edits`)
5. Consulted oracle (plan note 29baf270): strict guarantee for declared inputs, best-effort for worker-detected; mirrors Bazel/Buck

# Root Cause

**TOCTOU race:** Input resolution occurred post-execution. The resolved hashes reflected the filesystem state AFTER task completion, not what the task actually read. Content hashes were correct for the post-run state but wrong for the task's actual input consumption.

**Why mtime/size unsafe:** Mtime granularity can hide rapid edits (sub-second). Content hash comparison required.

# Solution

Implemented pre-execution snapshot with stability check in new module `input_stability.rs`:

## 1. Capture pre-execution snapshot

```rust
// CacheWriteContext stores pre-snapshot BEFORE task runs
pub struct CacheWriteContext {
    pre_snapshot: Vec<FileEntry>,  // declared inputs, content-hashed at T0
    // ...
}
```

## 2. Post-run stability check

```rust
pub(crate) fn check_input_stability(
    pre_snapshot: &[FileEntry],
    post_inputs: &[FileEntry],
    uses_worker_detected_patterns: bool,
    task_id: &TaskId,
) -> Result<Vec<FileEntry>, String>
```

- On mismatch: skip local cache write, skip shared publish, do NOT register watch state (task stays dirty), emit warning
- On match: return the VERIFIED PRE-SNAPSHOT as record.inputs (not post-run re-hash) — closes residual TOCTOU gap

## 3. Declared vs worker-detected asymmetry

```rust
// Declared-pattern run (uses_worker_detected_patterns = false):
// - ENTIRE post_inputs set is declared
// - Any file present post-run but absent from pre-snapshot → "new file" mismatch

// Worker-detected run (uses_worker_detected_patterns = true):
// - Only files ALSO in pre_snapshot get strict check
// - Remaining post-run files recorded best-effort (no pre-baseline available)
```

**Critical:** Do NOT seed pre-snapshot from prior record's stored detected-input hashes — legitimate between-run change would look like concurrent change, spuriously suppressing cache write.

## 4. Fail-safe for empty baseline

Empty pre-snapshot (pattern expansion failed) + non-empty declared post-run set → mismatch (conservative: skip caching rather than poison it).

# Why This Works

1. **Pre-snapshot is authoritative:** File hashes captured BEFORE execution reflect what the task actually reads (or what existed before it started)
2. **Post-run comparison detects concurrent edits:** If hashes differ, something changed during the run — reject cache write
3. **Pre-snapshot stored, not post-run:** Closes the gap between post-run resolution and actual cache-write; even a concurrent edit at that exact moment is detected
4. **Content hash comparison:** Mtime/size granularity can hide edits; content hash is definitive

# Prevention Strategies

## Test Cases

- **Concurrent edit during run:** Start task, modify input file mid-execution, verify cache write skipped
- **Declared input appeared mid-run:** New file matching declared glob appears during run → mismatch, cache write skipped
- **Worker-detected input stability:** Post-run detected input changed from prior run → should NOT spuriously suppress cache write (no pre-baseline)
- **Empty pre-snapshot fail-safe:** Pattern expansion fails, post-run non-empty → mismatch

## Code Review Checklist

- [ ] Is input resolution happening pre-execution (not post-run)?
- [ ] Is the stored record using pre-snapshot (not post-run re-hash)?
- [ ] Worker-detected inputs NOT seeded from prior record's stored hashes?
- [ ] Content hash comparison (not mtime/size)?

## Unit Test Gotchas

Input resolution walks up to git-repo boundary. Tests exercising real input resolution need `git init` in temp package dir; otherwise resolution returns empty and silently defeats test intent.

# CodeScene / Tooling Note

Luchta uses `cs delta origin/HEAD` as merge blocker. CodeScene analyzes LOGICAL Rust modules and follows `#[path = "..."] mod` includes. Moving a `mod tests` body into a `#[path]`-referenced file does NOT reduce the parent module's counted LOC (still same logical module) and can add "Low Cohesion" flag.

To reduce module LOC and fix "Lines of Code in a Single File" / "Large Method":
- Extract to GENUINE separate sibling module (`mod input_stability;`, not `#[path]` back into same module)
- Split large functions (extract-method)

# Related Issues

- **GitHub:** [#157](https://github.com/dobesv/luchta/issues/157)
- **Plan:** concurrent-change-handling
- **Related Solution:** [shared-build-cache-validate-on-restore-2026-06-17.md](shared-build-cache-validate-on-restore-2026-06-17.md) — shared cache restore validation uses similar input-only check
