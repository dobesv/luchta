---
title: "yarn.lock parsed once per run via three-state outcome enum"
date: 2026-06-15
category: "performance-issues"
problem_type: performance_issue
component: "cache-layer"
root_cause: "redundant disk read and parse in hot per-task path"
resolution_type: code_fix
severity: medium
tags:
  - rust
  - caching
  - performance
  - enum-pattern
  - refactoring
  - codescene
plan_ref: "issue-52-yarn-lock-cache"
---

## Problem

`gather_pkg_dep_pairs` read and parsed `yarn.lock` from disk for every cached task during both cache-skip-check and cache-write paths. On large monorepos with hundreds of tasks, this caused significant wasted I/O and CPU.

## Symptoms

- `fs::read_to_string` followed by `parse_lockfile` invoked once per task
- In monorepos with 100+ tasks, the same file read/parsed 100+ times per run
- Performance degradation noticeable on large workspaces
- No observable runtime failure — pure inefficiency

## Investigation Steps

1. Traced call graph: `run_tasks` → `dispatch_loop` → `build_cache_write_context` / `try_cache_skip` → `gather_pkg_dep_pairs` → `fs::read_to_string(workspace_root.join("yarn.lock"))` + `parse_lockfile`
2. Confirmed `yarn.lock` is workspace-global, not per-package — genuinely redundant per-task
3. Noted `package.json` reads are per-package — left unchanged (genuinely per-task state)
4. Analyzed existing behavior for four outcomes: missing/empty, parsed, parse error, I/O error
5. First refactor attempt: inline load block into `run_tasks`; CodeScene flagged "Large Method" (80 lines > 70 threshold)
6. Extracted `load_lockfile_state` helper to restore Code Health

## Root Cause

The original `gather_pkg_dep_pairs` design predated `DispatchContext` shared-state pattern. Each task independently loaded and parsed the same lockfile. Calculating dependency pairs required a lockfile lookup, but the implementation performed I/O inside the hot path rather than pre-loading once at orchestration level.

## Solution

Introduced module-private `LockfileState` enum and loaded it once in `run_tasks`:

```rust
pub(crate) enum LockfileState {
    Absent,
    Parsed(Arc<dyn Lockfile>),
    Failed(String),
}

pub(crate) fn load_lockfile_state(workspace_root: &Path) -> LockfileState {
    match fs::read_to_string(workspace_root.join("yarn.lock")) {
        Ok(contents) if contents.trim().is_empty() => LockfileState::Absent,
        Ok(contents) => match parse_lockfile(&contents) {
            Ok(parsed) => LockfileState::Parsed(Arc::<dyn Lockfile>::from(parsed)),
            Err(e) => LockfileState::Failed(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LockfileState::Absent,
        Err(e) => LockfileState::Failed(format!("failed to read yarn.lock: {e}")),
    }
}
```

`run_tasks` calls `load_lockfile_state(workspace_root)` once and stores result in `DispatchContext` as `lockfile: &'a LockfileState`. `gather_pkg_dep_pairs` dereferences:

```rust
pub(crate) fn gather_pkg_dep_pairs(
    package: &PackageNode,
    package_graph: Option<&PackageGraph>,
    lockfile: &LockfileState,
) -> Result<Vec<(String, String)>> {
    let lockfile = match lockfile {
        LockfileState::Absent => return Ok(Vec::new()),
        LockfileState::Failed(msg) => return Err(miette::miette!("{msg}")),
        LockfileState::Parsed(lf) => lf.as_ref(),
    };
    // ... existing logic with `lockfile` trait object
}
```

Both call sites (`build_cache_write_context` at line 900, `try_cache_skip` at line 1065) pass `ctx.lockfile` and keep existing `Err` match arms verbatim, preserving per-task warnings.

## Why This Works

**Three-state outcome enum preserves all behavior:**

| Original outcome | Mapped to | Behavior in `gather_pkg_dep_pairs` |
|---|---|---|
| Missing or empty `yarn.lock` | `Absent` | `Ok(Vec::new())` — empty dep pairs, caching proceeds |
| Successful parse | `Parsed(Arc<dyn Lockfile>)` | Normal dependency resolution |
| Parse error | `Failed(String)` | `Err(...)` — call sites warn + disable cache for that task |
| Non-NotFound I/O error | `Failed(String)` | `Err(...)` — same disable path |

Key insight: `Failed` surfaces as `Err`, so existing call-site `Err` arms fire unchanged. Per-task warnings preserved. Empty pairs on failure would risk false cache hits, so `Failed` must remain distinct from `Absent`.

`Arc<dyn Lockfile>` enables zero-copy sharing. `Box<dyn Lockfile>` is not `Clone`; `Arc` conversion provides single-ownership parse with read-only fan-out via borrowed `&LockfileState` in `DispatchContext`.

## Prevention Strategies

**Test Cases:**
- Unit test: `Absent` variant returns empty pairs
- Unit test: `Failed` variant returns `Err` with message preserved
- Integration test: missing `yarn.lock` still allows caching
- Integration test: unparseable `yarn.lock` disables cache per-task with warning

**Pattern to follow:**
When hoisting an operation out of a loop, encode the full outcome space in the shared value. Distinct failures must not collapse to a single "None" — call sites may require byte-for-byte identical branching logic.

Extract new logic into a named helper when inlining pushes the orchestrator function over code-health thresholds. CodeScene "Large Method" (70 lines) triggered after the ~12-line load block was inlined into `run_tasks`. Helper extraction kept `run_tasks` focused and restored Code Health score.

**Code Review Checklist:**
- [ ] Does the shared value encode all semantically-distinct outcomes?
- [ ] Are failure modes preserved (not collapsed to None/empty)?
- [ ] Is the trait object wrapped in `Arc` for sharing (not `Box`)?
- [ ] Does helper extraction avoid method-size thresholds?
- [ ] Do call-site `Err` arms remain unchanged (preserving warning locality)?

## Related Issues

- **GitHub:** [#52](https://github.com/dobesv/luchta/issues/52) — Cache: parse yarn.lock once per run
- **Related Solution:** [integration-issues/yarn-berry-lockfile-parser-2026-06-09.md](../integration-issues/yarn-berry-lockfile-parser-2026-06-09.md) — Lockfile trait and auto-detection design
- **Related Solution:** [logic-errors/uncached-task-detected-output-coupling-2026-06-12.md](../logic-errors/uncached-task-detected-output-coupling-2026-06-12.md) — DispatchContext shared-field pattern
- **Related Solution:** [workflow-issues/codescene-quality-score-refactoring-2026-06-09.md](../workflow-issues/codescene-quality-score-refactoring-2026-06-09.md) — CodeScene remediation patterns
