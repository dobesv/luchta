---
title: "CLI package targeting and CodeScene whack-a-mole resolution chain"
date: 2026-06-15
category: logic-errors
problem_type: logic_error
component: luchta-cli
root_cause: "CodeScene rule interactions require ordered refactoring; glob-based task selection silently dropped literal validation"
resolution_type: code_fix
severity: medium
tags:
  - cli
  - task-targeting
  - globset
  - codescene
  - refactoring-patterns
  - parameter-object
  - table-driven-tests
plan_ref: luchta-59-cli-task-targeting
last_updated: 2026-06-15
---

## Problem

Adding `-p/--package` task targeting to `luchta run` required satisfying multiple CodeScene quality rules simultaneously. The rules interacted: fixing one often triggered another, leading to thrashing when approached naïvely. Additionally, the glob-based selection model silently dropped explicit literal task targets when other tasks matched.

## Symptoms

- **CodeScene whack-a-mole**: Excess Function Arguments fix created Large Method; Large Method extraction triggered Code Duplication; table-driven consolidation created new Large Method; helpers formed disconnected LCOM4 clusters.
- **Literal validation regression**: `luchta run build missing-task` succeeded if `build` existed, silently ignoring `missing-task`. Pre-feature behavior errored on unmatched literals.
- **Eval-order edge case**: `-T -p no-such-pkg build` reported "No tasks matched" instead of "No packages matched" when package check happened after task matching.

## Investigation Steps

1. Traced CodeScene deltas: `cs delta origin/HEAD` showed each triggered rule after each refactor.
2. Analyzed Code Duplication in test file: ~12 `collect_requested_subgraph_*` tests with near-identical structure.
3. Identified LCOM4 cluster formation: shared helper `run_selection_case` disconnected from test module context.
4. Traced literal validation regression: pre-feature code looped over each requested task individually; glob refactor switched to set-based matching that only checked `requested_ids.is_empty()`.

## Root Cause

**CodeScene interaction**: The Excess-Arguments threshold (4 params), Large Method (70 lines), Code Duplication (identical code blocks), and LCOM4 cohesion rules form a constraint graph. Fixes must be ordered to avoid violating previously-satisfied constraints.

**Literal validation**: Glob-based selection uses "match-any" semantics (correct for wildcards), but was incorrectly applied to all task arguments including explicit literals. The `is_literal_pattern` check was missing, so typos in multi-task commands were silently tolerated.

## Solution

### 1. Parameter Object Pattern for Excess-Args

Encapsulated selection params into structs to reduce function argument counts:

```rust
/// User's task selection from CLI arguments.
#[derive(Debug)]
pub struct TaskSelection<'a> {
    pub requested_tasks: &'a [String],
    pub packages: &'a [String],
    pub top_level: bool,
}

/// Internal criteria for matching task nodes.
struct SelectionCriteria<'a> {
    task_globs: &'a GlobSet,
    package_globs: &'a GlobSet,
    match_all_non_root_packages: bool,
    top_level: bool,
}
```

Final signatures:
- `run_tasks(workspace_root, selection, output)` — 3 args
- `collect_requested_subgraph(task_graph, selection, pruned)` — 3 args
- `collect_matching_task_ids(available_nodes, criteria)` — 2 args
- `package_matches(task_id, criteria)` — 2 args

Mirrors `build_globset` precedent in `luchta-cache/resolve.rs`.

### 2. CodeScene Resolution Chain

The ordering that worked:

1. **Excess-Args**: Introduce `TaskSelection<'a>` and `SelectionCriteria<'a>` structs
2. **Large Method (run_tasks)**: Extract `build_run_executor` helper for executor/reporter setup
3. **Code Duplication**: Collapse ~12 duplicated tests into table-driven tests with const case tables + shared `run_selection_case` helper
4. **Large Method (test table)**: Move case data into top-level `const` definitions outside the helper
5. **LCOM4 cohesion**: Move entire matrix-test block (type aliases, const tables, helper, 2 tests) into `#[path]` submodule `src/run/tests/run_selection_matrix_tests.rs`. Fixtures stay in parent, accessed via `use super::*`.

**Code Health improved 7.72 → 8.03** with zero new/degraded issues.

### 3. Literal Task Validation Restoration

Added per-literal validation that runs after glob matching but preserves package-first eval order:

```rust
fn is_literal_pattern(s: &str) -> bool {
    !s.contains(['*', '?', '[', ']', '{', '}', '!', '\\'])
}

fn validate_literal_task_requests(
    requested_ids: &HashSet<TaskId>,
    selection: &TaskSelection<'_>,
    pruned: &[PrunedTask],
) -> Result<()> {
    for requested in selection.requested_tasks.iter()
        .map(String::as_str)
        .filter(|requested| is_literal_pattern(requested))
    {
        let matched = requested_ids.iter()
            .any(|task_id| task_id.task.as_str() == requested);
        if !matched {
            report_unmatched_request(requested, pruned, selection.top_level)?;
        }
    }
    Ok(())
}
```

Evaluation order in `collect_requested_subgraph`:

1. **Package match check first**: `if !packages.is_empty() && matched_package_names.is_empty()` → bail
2. **Task matching**: `collect_matching_task_ids(...)`
3. **Per-literal validation**: `validate_literal_task_requests(...)` — errors on unmatched literals
4. **Empty goal fallback**: Generic "No tasks matched filter" when no globs matched

Glob patterns retain "match-any" semantics; literals require at least one match.

### 4. Selection Matrix Implementation

Four-way predicate for `-T` vs `-p` interaction in `package_matches`:

| `-T` | `-p` provided | Result |
|------|---------------|--------|
| true | true | matched non-root packages + root |
| true | false | root only |
| false | true | matched non-root packages only |
| false | false | all non-root packages |

Root sentinel `//root` gated ONLY by `-T`; package globs never admit root.

## Why This Works

### Parameter Object Pattern

- Bundles related arguments, improving call-site readability
- Reduces argument count below CodeScene's 4-argument threshold
- Precedent in codebase (`build_globset`) reduces surprise

### Table-Driven Tests in Submodule

- `#[path]` directive keeps test data in separate file without creating a separate crate
- Fixtures remain in parent module, accessed via `use super::*`
- Case tables (`const SUCCESS_CASES`, `const ERROR_CASES`) are compile-time constants
- Single helper `run_selection_case` processes all cases, eliminating duplication

### Literal Validation Ordering

- Package-first error provides earliest useful feedback
- Per-literal validation catches typos even when other tasks match
- Glob patterns unaffected: `*build*` matching nothing produces "No tasks matched" without per-pattern error
- Preserved `expand_with_dependencies` untouched: prerequisites in unmatched packages still run (goal-not-filter model)

## Prevention Strategies

**Refactoring Checklist (when CodeScene rules interact):**

- [ ] Identify all triggered rules before starting
- [ ] Order fixes: parameter objects → method extraction → table consolidation → module splitting
- [ ] Run `cs delta` after each change, not just at the end
- [ ] When table tests hit Large Method, move case data to top-level `const`
- [ ] When helpers form LCOM4 clusters, consider `#[path]` submodule extraction

**CLI Selection Semantics:**

- [ ] Literal arguments validate individually; globs use match-any
- [ ] Error evaluation order: package filters first, then task matching
- [ ] Goal-not-filter: selection picks goals; `expand_with_dependencies` still expands all prereqs
- [ ] Root sentinel (`//root`) never matches package globs; only admitted via `-T`

**Testing Patterns:**

- [ ] Table-driven tests for selection matrices
- [ ] Explicit eval-order tests: package-miss-before-task-miss
- [ ] Per-literal validation tests: `run build missing` should error
- [ ] Goal-not-filter tests: prereqs in unmatched packages still included

## Related Issues

- **Related Solution:** [root-task-exclusion-and-global-expansion-skip-2026-06-15.md](root-task-exclusion-and-global-expansion-skip-2026-06-15.md) — Root task gating and `expand_with_dependencies` invariants
- **Related Solution:** [codescene-quality-score-refactoring-2026-06-09.md](../workflow-issues/codescene-quality-score-refactoring-2026-06-09.md) — General CodeScene remediation patterns
- **GitHub:** [#59](https://github.com/dobesv/luchta/issues/59) — Improve task targeting on the command line
