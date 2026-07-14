---
title: "Parallelizing in-process worker file processing (oxlint, oxfmt, swc/oxc-transform)"
date: 2026-07-13
category: performance-issues
problem_type: performance_issue
component: luchta-workers
root_cause: serial per-file loop awaiting spawn_blocking or calling batch-capable API with 1-element batch
resolution_type: code_fix
severity: high
tags:
  - parallelism
  - thread-scope
  - spawn-blocking
  - workers
  - rayon-alternative
  - tsgolint-batching
plan_ref: oxlint-oxfmt-worker-perf
last_updated: 2026-07-14
---
# Parallelizing in-process worker file processing

## Problem

Four luchta in-process workers processed files serially, making them ~3.4–3.6x slower than parallel on multi-core (measured 24-core, ~495-file corpus). oxc's `run_source` parallelizes internally via rayon, but workers called it one file at a time. oxfmt/swc/oxc-transform awaited `spawn_blocking` inside a for-loop = one file at a time.

## Symptoms

```
- oxlint worker: 3.39x slower than batched run_source (median 0.0858s vs 0.0254s)
- oxfmt worker: 3.62x slower than parallel format_path (median 0.0724s vs 0.0200s)
- swc-transform/oxc-transform: same serial await pattern
- Diagnostic tip: per-file loop that awaits a single spawn_blocking, or calls a batch-capable API with a 1-element batch
```

## Investigation Steps

1. Hypothesis: CLI uses more parallelism than worker. Measured probe binaries mimicking worker code.
2. oxlint probe: `for file in files { service.run_source(vec![single_path]) }` — batching all paths reduced median 0.0858s → 0.0254s.
3. oxfmt probe: `for path { spawn_blocking(format_path).await }` — rayon par_iter reduced median 0.0724s → 0.0200s.
4. Control `RAYON_NUM_THREADS=1`: speedups vanished → confirms parallelism is the factor.
5. Audited all in-process workers: oxlint, oxfmt, swc-transform, oxc-transform all serial; ast-grep-worker already parallel via `thread::scope`.

## Root Cause

Two patterns:

1. **oxlint**: looped `service.run_source(os_fs, vec![single_path])` once per file. oxc's `run_source` parallelizes across PATHS via internal rayon::scope → 1 path = zero parallelism.
2. **oxfmt/swc-transform/oxc-transform**: `for file in files { spawn_blocking(work).await }` — awaiting each spawn_blocking sequentially = one file at a time.

Constraint: AGENTS.md forbids rayon in luchta's own code. oxc's transitive rayon (inside run_source/formatter/transform) is acceptable.

## Solution

Apply `std::thread::scope` + `available_parallelism()` chunking pattern (same as luchta-ast-grep-worker `scan_files_with_collection`):

```rust
let threads = available_parallelism().min(files.len().max(1));
let chunk_size = files.len().max(1).div_ceil(threads);
std::thread::scope(|scope| {
    for chunk in files.chunks(chunk_size) {
        scope.spawn(move || {
            for file in chunk { /* process file */ }
        });
    }
});
// join happens automatically at scope end
```

**Key implementation details:**

1. Wrap entire parallel phase in ONE `tokio::task::spawn_blocking`, do all CPU + independent file writes inside scoped threads.
2. Defer `ctx`/`JobContext` emits to async side AFTER join — JobContext isn't usable across threads.
3. Collect per-file OUTCOME (enum) or `(produced, errors, failed)` tuple per chunk.
4. Replay/emit in ORIGINAL FILE ORDER (join chunks in spawn order; inputs pre-sorted) so stdout/stderr/SARIF stay deterministic.

**oxlint type-aware path (Design A → superseded):**

Initial fix: parallelize ONLY the standard (non-type-aware) path via thread::scope; keep type-aware path serial because it spawns an external process. This helped the non-tsgolint path but did NOT fix the ~10x slowdown reported when type-aware linting was enabled. The real bottleneck was the tsgolint invocation pattern, not whether it ran in parallel. See "Type-aware tsgolint batching" section below for the proper fix.

**Gotcha: unnameable `DiffManager`:**

oxc's `DiffManager` type is re-exported only from a private module (`mod suppression`), so external crates can't name it — cannot write a helper `fn(diff: &DiffManager)`. Workaround: keep the per-file body INLINE in the `scope.spawn` closure; closures capture `diff: Arc<DiffManager>` by reference via inference without naming the type.

## Type-aware tsgolint Batching (~36x speedup)

The thread::scope fix (above) addressed standard linting but type-aware linting remained ~36x slower than oxlint CLI with `--type-aware`. Root cause: per-file tsgolint process invocation.

### Problem

Worker invoked tsgolint (Go binary, `tsgolint headless`) ONCE PER FILE via `TsGoLintState::lint_source(vec![single_path])` in a loop. Each invocation:
1. Spawns a tsgolint process (~5-10ms)
2. Loads the TypeScript type-checker PROGRAM from scratch (~100-500ms) — DOMINANT cost
3. Lints that single file
4. Exits

For a 60-file package: 60 process spawns + 60 TS program loads. oxlint CLI invokes tsgolint ONCE for all files (loads TS program once, tsgolint parallelizes internally). That's the ~36x difference.

tsgolint is strictly one-shot: write JSON payload to stdin, close stdin (EOF), it lints all files, streams stdout, exits. No persistent/server mode exists — maintainers explicitly rejected it (oxc-project/tsgolint issue #71).

### Root Cause

Two compounding factors:
1. `TsGoLintState::lint_source` returns a flat `Vec<Message>` that DISCARDS file_path — unusable for batching in a per-file output model
2. Worker looped over files calling `lint_source(vec![single_path])` instead of batching all paths in one call

### Solution: Batch Per Package

Use oxc's PUBLIC `LintRunner::lint_files` — the CLI's orchestrator — which batches BOTH standard rules (LintService::run) AND tsgolint over the whole package in one pass, sharing one `Arc<DiffManager>` + one `DiagnosticSender` (mpsc) channel.

**Why per-package (per-tsconfig), NOT repo-wide:**
- tsgolint groups files by tsconfig internally anyway → no gain from repo-wide
- Repo-wide risks OOM/GC thrashing on large repos (tsgolint issue #67; ProjectService removed in PR #139)
- Preserves luchta's per-package task/cache/scheduling model — packages run concurrently under weight scheduler
- Each package has its own tsconfig/type-program regardless

**Implementation:**
```rust
// Build LintRunner with type-aware enabled
let linter = Linter::new(LintOptions::default(), store, None).with_fix(FixKind::None);
let runner = LintRunner::builder(options, linter)
    .with_type_aware(type_aware)
    .with_type_check(type_check)
    .with_silent(false)
    .with_fix_kind(FixKind::None)
    .with_timings(false)
    .build()?;  // HARD-FAILS if tsgolint missing + type_aware=true

// Run over all package files
let (tx, rx) = mpsc::channel::<Vec<Error>>();
runner.lint_files::<false>(&paths, tx.clone(), &diff, None)?;

// Collect diagnostics
let errors: Vec<Error> = rx.try_iter().flatten().collect();
```

Collect `Vec<Error>` from the channel, convert via `Info::new` to `WrappedDiagnostic` (info.filename = relative uri), sort by `(uri, line, col, rule, msg)` for determinism.

### Key Gotchas

1. **LintRunnerBuilder HARD-FAILS** if `with_type_aware(true)` but tsgolint binary is missing. Preflight with `TsGoLintState::try_new(...).is_ok()` and fall back to `with_type_aware(false)` + a warning.

2. **collect_file is called once per file** by both standard and tsgo passes — INTENDED and correct. `RuntimeSuppressionMap.merge_file` keyed by `(file, rule)`; standard vs tsgo rule names disjoint (oxc comment: "counts from both oxlint and tsgo passes"). Don't add extra collect_file calls.

3. **FixKind::None preserves old behavior.** The old worker built Linter WITHOUT `.with_fix`, so messages carried no fixes. Setting `FixKind::None` maintains parity. Enabling real fixes is a separate behavior change.

### Tuning Note

tsgolint PR #1019 caps checker pool to `min(4, GOMAXPROCS)` to avoid Go GC CPU-pinning under high concurrency. luchta runs package lint tasks concurrently, so this cap may already be active.

### Measured Impact

- 60-file type-aware fixture, 24-core host
- Before: 2.41s (per-file tsgolint invocations)
- After: 0.067s (batched via LintRunner::lint_files)
- **Speedup: ~36x**

Exceeds user-reported 10x slowdown (per-file CLI comparison). Both produce identical findings (verified type-aware rule hits across files, sorted output).

## Why This Works

- `std::thread::scope` allows borrowing stack variables across scoped threads; threads are joined at scope end.
- `available_parallelism()` + chunking respects CPU count without rayon dependency.
- Thread-safety facts: `LintService::run_source` is `&self`; `DiffManager` (Arc) `collect_file`/`collect_empty_file` are `&self` with interior mutability (designed for concurrent use); file writes go to distinct paths.

## Prevention Strategies

**Code Review Checklist:**

- [ ] Is there a per-file loop awaiting a single `spawn_blocking`?
- [ ] Is a batch-capable API called with 1-element batches?
- [ ] Should the loop be parallelized via `thread::scope` + chunking?
- [ ] For type-aware linting: is tsgolint invoked once per file (WRONG) or once per package (CORRECT)?
- [ ] Does the batching strategy match the scheduling boundary (per-package for luchta)?

**Best Practices:**

- Use `std::thread::scope` + `available_parallelism().min(files.len().max(1))` chunking for CPU-bound file processing.
- Keep parallel CPU work inside ONE `spawn_blocking`; defer async-side emissions to after join.
- Preserve deterministic output order: sort inputs, join chunks in spawn order, emit results sequentially.
- For external process batching: invoke once per natural grouping (per-tsconfig/package for tsgolint), NOT repo-wide or per-file.
- Verify suppression semantics when batching — `collect_file` must be called exactly once per file by each pass (standard + tsgo), and rule names must be disjoint.

**Test Considerations:**

- Run tests with `RAYON_NUM_THREADS=1` to verify correctness when parallelism is disabled.
- Assert exact output order (stdout/stderr/SARIF) to catch non-determinism.
- For tsgolint batching: assert only ONE process spawn for multi-file package (or that batched code path is exercised).
- Verify diagnostic output parity between batched and per-file approaches.

## Related Issues

- **Plan:** oxlint-oxfmt-worker-perf
- **Reference impl:** luchta-ast-grep-worker `scan_files_with_collection`
- **Related Solution:** [integration-issues/ast-grep-worker-in-process-integration-2026-07-11.md](../integration-issues/ast-grep-worker-in-process-integration-2026-07-11.md) — ast-grep worker's `thread::scope` pattern
- **Related Solution:** [performance-issues/remote-cache-parallel-sync-nested-runtime-2026-06-24.md](remote-cache-parallel-sync-nested-runtime-2026-06-24.md) — `std::thread::scope` for nested runtime safety
- **tsgolint issues:** oxc-project/tsgolint #67 (OOM), #71 (server mode rejected), #139 (ProjectService removed), #1019 (checker pool cap)
- **oxc reference:** `LintRunner::lint_files` in `oxlint_runner.rs` — CLI's batched orchestrator
