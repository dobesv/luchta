---
title: "Task list subcommand: selection semantics, display patterns, and cross-crate struct extension"
date: 2026-07-20
category: logic-errors
problem_type: logic_error
component: luchta-cli
root_cause: "Established patterns for task selection and display; struct field extension broke non-defaulting literal sites"
resolution_type: code_fix
severity: medium
tags:
  - task-selection
  - pruned-tasks
  - sparse-output
  - serde-default
  - cross-crate-compatibility
  - root-task-display
plan_ref: luchta-list-subcommand
---

## Problem

Implementing `luchta list` required choosing between two existing task-selection patterns (runnable-only versus explain-everything), deciding how to render sparse human output while preserving stable JSON schema, and extending `TaskDefinition` without breaking struct-literal constructions across the workspace.

## Symptoms

- Adding `description: Option<String>` to `TaskDefinition` broke compile in `luchta-cache/src/hashing.rs` and `luchta-cli/src/env_conflict.rs` — explicit struct literals without `..Default::default()` spread.
- Root-task headers initially rendered as `##task` due to naive `package_and_task_display` usage.
- `why` command shares identical root-task display bug (latent, predates this change).

## Root Cause Analysis

### 1. Selection pattern mismatch

Two established task-selection patterns exist:
- **Runnable-only** (`run`, `logs`): uses `collect_requested_subgraph(expand_dependencies: false)` via `task_graph.nodes()`, excludes pruned tasks
- **Explain-everything** (`why`): additionally calls `matching_pruned_ids()` to include and explain pruned tasks

`list` must model on runnable-only to match user expectation: list tasks you could actually run.

### 2. Display pattern pitfall

`package_and_task_display(task_id)` returns `("#", task)` for root tasks (with `#` as the package marker). Naively joining with `{package}#{task}` yields `##task`. The correct approach is printing `{task_id}` directly — `TaskId::Display` already handles `#task` vs `pkg#task`.

### 3. Cross-crate struct extension hazard

Most `TaskDefinition` construction sites use `..Default::default()` and inherited the new field automatically. Sites using explicit field lists without spread syntax fail compile when a new field is added, even if default-constructible.

## Solution

### 1. Selection logic mirrors `logs.rs`

```rust
// crates/luchta-cli/src/list.rs
fn select_task_ids(
    task_graph: &TaskGraph,
    selection: &TaskSelection,
    pruned: &HashSet<TaskId>,
) -> Vec<TaskId> {
    if selection.uses_default_scope() {
        // All non-pruned nodes
        task_graph.nodes().iter()
            .filter(|n| !pruned.contains(&n.id))
            .map(|n| n.id.clone())
            .collect()
    } else if selection.requests_only_top_level() {
        // Root tasks only
        task_graph.nodes().iter()
            .filter(|n| n.id.is_root() && !pruned.contains(&n.id))
            .map(|n| n.id.clone())
            .collect()
    } else if selection.requests_packages_without_tasks() {
        // Package-filtered non-root tasks
        let matched_packages = collect_matched_package_names(...);
        task_graph.nodes().iter()
            .filter(|n| matched_packages.contains(&n.id.package) && !n.id.is_root())
            .map(|n| n.id.clone())
            .collect()
    } else {
        collect_requested_subgraph(task_graph, selection, pruned)
    }
}
```

Key difference from `why.rs`: never calls `matching_pruned_ids()`.

### 2. Sparse output via separate formatter

Human output omits default-valued fields (weight=1, dependencies=["\*\*/*\*"], empty collections, None options). JSON output includes all fields with explicit nulls.

```rust
fn format_non_default_fields(task_def: &TaskDefinition) -> Vec<(&'static str, String)> {
    let mut fields = Vec::new();
    if task_def.description.is_some() { fields.push(("description", task_def.description.as_ref().unwrap())); }
    if task_def.weight != 1 { fields.push(("weight", task_def.weight.to_string())); }
    if !task_def.dependencies.is_empty() && task_def.dependencies != vec!["**/*"] {
        fields.push(("dependencies", format!("{:?}", task_def.dependencies)));
    }
    // ... other fields
    fields
}
```

Serde config: `#[serde(default)]` only — no `skip_serializing_if`. Explicit nulls provide stable JSON schema.

### 3. Root-task header fix

**Before (buggy):**
```rust
let (package, task) = package_and_task_display(&task_id);
println!("{package}#{task}");  // → "##task" for root!
```

**After:**
```rust
println!("{task_id}");  // TaskId::Display → "#task" or "pkg#task"
```

### 4. Cross-crate struct extension pattern

After adding a field to a shared type, run workspace-wide build:
```bash
cargo build --workspace --tests
```

Two sites required manual fix:
- `crates/luchta-cache/src/hashing.rs:595`
- `crates/luchta-cli/src/env_conflict.rs:159,215,328`

All other sites used `..Default::default()` and compiled cleanly.

## Why This Works

1. **Selection consistency**: `list` behaves like `logs` and `run` — users see runnable tasks only. `why` is the exception (explains pruning), not the model.

2. **Stable JSON schema**: `#[serde(default)]` without `skip_serializing_if` ensures JSON consumers always see consistent field presence. Human sparseness is a display concern decoupled from serialization.

3. **TaskId encapsulation**: `TaskId::Display` is the authoritative rendering. Helper `package_and_task_display` exists for cases requiring separate components; direct display avoids reconstruction bugs.

4. **Spread syntax resilience**: Sites using `..Default::default()` automatically inherit new fields. Only explicit literals require manual update.

## Prevention Strategies

**Test Cases:**
- Integration tests for `-T` (top-level), default scope, `-p <pkg>` selection modes
- Root-task header assertion: verify `#task` not `##task`
- Cross-crate struct-literal sites: add field to shared type, run `cargo build --workspace --tests`

**Best Practices:**
- New query commands should start from runnable-only selection (`logs.rs` pattern), not explain-everything (`why.rs`)
- Prefer `{task_id}` display over manual `package#task` reconstruction
- Use `..Default::default()` for struct literals unless intentional full-field specification
- Run workspace-wide build after adding fields to shared types

**Code Review Checklist:**
- [ ] Selection logic mirrors runnable-only commands if listing executable tasks?
- [ ] Root task headers use `TaskId::Display` directly?
- [ ] Struct literals in tests/util code use `..Default::default()`?
- [ ] After adding shared-type field, `cargo build --workspace --tests` passes?

## Related Issues

- **GitHub:** [#14](https://github.com/dobesv/luchta/issues/14) — Add list subcommand
- **Related Solution:** [root-task-exclusion-and-global-expansion-skip-2026-06-15.md](root-task-exclusion-and-global-expansion-skip-2026-06-15.md) — Root task handling in task graph
- **Related Solution:** [dry-run-display-filtering-pruned-tasks-2026-06-25.md](dry-run-display-filtering-pruned-tasks-2026-06-25.md) — Pruned task filtering semantics
- **Related Solution:** [cache-nonce-invalidation-control-2026-06-23.md](cache-nonce-invalidation-control-2026-06-23.md) — Workspace build after struct field addition
