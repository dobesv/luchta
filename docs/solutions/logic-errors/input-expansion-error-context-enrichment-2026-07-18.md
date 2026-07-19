---
title: "Input expansion error context enrichment: provenance tracking + sentinel-safe display"
date: 2026-07-18
category: logic-errors
problem_type: logic_error
component: luchta-engine/input-expansion
root_cause: "Error diagnostics lacked task identity and input provenance; internal sentinel leaked via PackageName Display"
resolution_type: code_fix
severity: medium
tags:
  - error-messages
  - provenance
  - sentinel
  - worker-modification
  - diagnostics
  - TaskModification
plan_ref: issue-83-input-escape-error-context
---

## Problem

Input expansion errors (path escapes, unknown packages, invalid patterns) only named the package, not the specific task. Users couldn't tell which task triggered the error or whether the offending input pattern came from the original task spec or from a worker's `TaskModification.inputs` override. Additionally, the internal `//root` package sentinel (documented as "must never be shown to users") leaked through `PackageName` Display in user-facing messages for root tasks.

## Symptoms

```
✖ input "/" in package "@formative/node-pagination": path escape in input pattern '/' from package '@formative/node-pagination': resolved path escapes base directory
```

Observable problems:

- No task identifier in the error message
- No indication whether input came from task spec or worker
- Root tasks showed `//root` sentinel in error output
- Four emission paths (cache-write, eager validation, read/skip, pre-execution warnings) had inconsistent context

## Investigation Steps

1. Traced `InputExpansionError` emission through CLI: `resolve_cache_inputs` (write path), `build_command_map` (eager path), `format_expansion_error` (read path), `resolve_pre_execution_inputs` (pre-execution warnings).

2. Identified that `TaskModification.inputs` fully replaces declared inputs when `Some`, but provenance was lost after `apply_to` ran. The replacement happens in `ResolvedPipeline::resolve` at `task_graph.rs:797-800`.

3. Found `PackageName::Display` writes raw `.as_str()` with no special handling for root, while `TaskId::Display` correctly renders root tasks as `#task`.

4. Discovered four emission sites, some printing raw `source_pkg` directly, bypassing any sentinel protection.

5. Cycle 2 review found pre-execution warning path still leaked `//root` via direct `{source_pkg}` interpolation.

## Root Cause

Provenance tracking was absent from `TaskGraph` — the information about whether inputs were worker-replaced was lost immediately after `TaskModification.apply_to` flattened the modification. Error display formatted `PackageName` directly without masking the internal sentinel.

## Solution

**Provenance tracking (task_graph.rs):**

Added `inputs_from_worker_by_id: HashMap<TaskId, bool>` to both `ResolvedPipeline` and `TaskGraph`. Populated at the single mutation point:

```rust
// task_graph.rs:807-809
ResolveDecision::Modify(modification) => {
    if modification.inputs.is_some() {
        self.inputs_from_worker_by_id.insert(task_id.clone(), true);
    }
    // ...
}
```

Exposed via `TaskGraph::inputs_from_worker(&TaskId) -> bool` (defaults to `false` for unknown tasks).

**Sentinel-safe error Display (input_expansion.rs):**

Added private helper and applied to all variants:

```rust
fn display_package(package: &PackageName) -> String {
    if package.as_str() == ROOT_PACKAGE_NAME {
        "the workspace root".to_string()
    } else {
        format!("package '{}'", package.as_str())
    }
}

#[error("path escape in input pattern '{pattern}' from {}: ...", display_package(source_pkg))]
PathEscape { ... }
```

**CLI context propagation (input_stability.rs, dispatch.rs):**

Shared helper for origin clause:

```rust
pub(crate) fn input_origin_clause(inputs_from_worker: bool) -> &'static str {
    if inputs_from_worker { "returned by the worker" }
    else { "declared in the task spec" }
}
```

Write path message format:

```
input "{pattern}" for task "{task_id}" ({origin}): {error}
```

Pre-execution warnings updated to use `task_id` (safe) and origin clause instead of raw `source_pkg`.

**Function argument count mitigation (input_stability.rs):**

Adding 2 params to `resolve_pre_execution_inputs` pushed it to 6 args (CodeScene max 4). Encapsulated in borrowed struct:

```rust
pub(crate) struct PreExecutionSnapshotRequest<'a> {
    pub input_patterns: &'a [String],
    pub source_pkg: &'a PackageName,
    pub package_graph: &'a PackageGraph,
    pub repo_root: &'a Path,
    pub task_id: &'a TaskId,
    pub inputs_from_worker: bool,
}
```

## Why This Works

- **Capture provenance at mutation point**: Before `TaskModification.apply_to` flattens, we know whether `inputs` was `Some`. After flattening, that information is lost. Recording at the mutation site preserves truth.

- **Fix sentinel leak at source**: `InputExpansionError::Display` is the single source of truth for error text. By routing through `display_package()`, ALL consumers get safe output without per-call-site fixes.

- **Use TaskId for package context**: Root tasks render as `#task`, never leaking `//root`. Including `task_id` provides safe package context; a separate raw package clause is both leaky and redundant.

- **Shared helper prevents divergence**: `input_origin_clause` in `input_stability.rs` (made `pub(crate)`) ensures write and eager paths stay in sync.

- **Struct encapsulation for arg count**: `PreExecutionSnapshotRequest<'a>` borrows params without copying, satisfying CodeScene's "Excess Number of Function Arguments" rule.

## Prevention Strategies

**Test cases:**
- Expansion error includes task ID in message
- Expansion error distinguishes worker vs task-spec origin
- Root task error never shows `//root` sentinel
- Non-input modifications don't set `inputs_from_worker`

**Code review checklist:**
- [ ] Does new provenance metadata get captured at the mutation point?
- [ ] Are all error emission paths audited for sentinel safety?
- [ ] Does `TaskId` handle root rendering safely (prefer over raw package)?
- [ ] When adding params, check CodeScene "Excess Number of Function Arguments" — encapsulate if approaching limit.

**Best practices:**
- Track provenance where mutation happens, not where error surfaces
- Fix sentinel/identifier leaks at the error type's `Display`, not just call sites
- Audit ALL emission paths — leak fixed in one wrapper persists via wrapped error's Display
- Use borrowed structs to stay under function argument limits

## Related Issues

- **GitHub Issue:** #83 — Error about input escaping package does not specify which task
- **Related Solution:** [security-issues/cross-package-input-expansion-security-2026-06-16.md](../security-issues/cross-package-input-expansion-security-2026-06-16.md) — Hard-fail on untrusted worker input patterns
