---
title: "Root task exclusion from default run and global task expansion skip"
date: 2026-06-15
category: logic-errors
problem_type: logic_error
component: luchta-cli, luchta-engine, luchta-workspace
root_cause: "Two distinct root package notions conflated; PackageGraph::with_root_package never wired"
resolution_type: code_fix
severity: high
tags:
  - task-graph
  - root-tasks
  - global-expansion
  - package-graph
  - pipeline-keys
plan_ref: luchta-top-level-tasks
---

## Problem

`luchta run <task>` matched and ran top-level (workspace-root) tasks by default, causing unexpected whole-repo builds when a root `build` task existed. Additionally, bare global task specs in config (e.g., `build: { worker: "yarn" }`) expanded onto the actual root package, creating duplicate root-task nodes.

Root cause: two distinct "root" package notions were conflated, and the mechanism to track the actual root package (`PackageGraph::root_package()`) was never wired in production code.

## Symptoms

```text
luchta run build   # unexpectedly runs //root#build (recursive whole-repo build)
```

- Global `build` task materialized as both `//root#build` (synthetic) and `my-repo#build` (actual root)
- Users unable to run package-only builds without root task interference
- Integration tests counting "Done: 3 tasks" when only 2 package tasks expected

## Investigation Steps

1. Reviewed prior art (`task-graph-scoped-keys-and-dependency-resolution-2026-06-10.md`) showing `PipelineKey::Root/Package/Global` parsing already correct in `parse_pipeline_entries`.

2. Found `PackageGraph::with_root_package` and `root_package()` methods existed but `with_root_package` was never called — `prepare_workspace` built the graph via `PackageGraph::build(packages)` only, leaving `root_package()` always `None`.

3. Traced `ResolvedPipeline::build` in task_graph.rs: global task expansion iterated all packages in topological order without any root exclusion.

4. Found CLI `collect_matching_task_ids` matched by task name only, selecting both `//root#build` and `pkg#build` for `luchta run build`.

## Root Cause

### Two Distinct "Root" Notions

**Actual root package**: A real named package discovered at workspace root (e.g., `my-repo`). Lives in `PackageGraph` as a normal node with edges to dependencies.

**Synthetic `//root`**: `ROOT_PACKAGE_NAME` constant (`"//root"`) used only for `#task` config keys. Creates `TaskId` with `is_root() == true`. NOT in `PackageGraph`.

Both were excluded from default operations, but via different mechanisms that needed explicit wiring:

- Synthetic `//root` excluded by CLI filter: `node.id.is_root() == top_level` (default `top_level=false`)
- Actual root package excluded by engine guard: `package_graph.root_package() == Some(&package_name)`

### The Hidden Gap

`PackageGraph::with_root_package` existed but was never called in production:

```rust
// crates/luchta-workspace/src/package_graph.rs
pub fn with_root_package(mut self, name: PackageName) -> Self {
    self.root_package = Some(name);
    self
}

pub fn root_package(&self) -> Option<&PackageName> {
    self.root_package.as_ref()
}
```

`prepare_workspace` built the graph without it, so `root_package()` returned `None` and the engine guard never triggered.

## Solution

### 1. Wire root package in `prepare_workspace`

In `crates/luchta-cli/src/run.rs`:

```rust
pub async fn prepare_workspace(
    workspace_root: &Path,
    mode: ResolveMode,
) -> Result<PreparedWorkspace> {
    let workspace = YarnWorkspace::new(workspace_root);
    let packages = workspace.discover()?;
    
    // NEW: Find root package by path comparison
    let root_package = packages
        .iter()
        .find(|package| package.path == workspace_root)
        .map(|package| package.name.clone());
    
    let package_graph = PackageGraph::build(packages.clone())?;
    
    // NEW: Wire root package if found
    let package_graph = if let Some(root_package) = root_package {
        package_graph.with_root_package(root_package)
    } else {
        package_graph
    };
    // ...
}
```

### 2. Skip global task expansion for root package

In `crates/luchta-engine/src/task_graph.rs`, `ResolvedPipeline::build`:

```rust
for package_name in package_graph
    .topological_order()?
    .into_iter()
    .map(|package| package.name.clone())
{
    let mut task_names = HashSet::new();
    let skip_global_tasks_for_root = package_graph.root_package() == Some(&package_name);

    // NEW: Guard prevents global tasks from expanding onto actual root
    if !skip_global_tasks_for_root {
        for (task_name, definition) in &global_tasks {
            let task_id = TaskId::new(package_name.clone(), task_name.clone());
            tasks_by_id.insert(task_id, definition.clone());
            task_names.insert(task_name.clone());
        }
    }

    // Package-scoped and root-scoped paths unchanged
    for ((package, task_name), definition) in &package_tasks {
        if package == &package_name {
            let task_id = TaskId::new(package_name.clone(), task_name.clone());
            tasks_by_id.insert(task_id, definition.clone());
            task_names.insert(task_name.clone());
        }
    }
}
```

### 3. CLI filter for root task selection

In `crates/luchta-cli/src/run.rs`, `collect_matching_task_ids`:

```rust
fn collect_matching_task_ids(
    available_nodes: &[&TaskNode],
    requested: &str,
    requested_ids: &mut HashSet<TaskId>,
    top_level: bool,
) -> bool {
    let mut matched = false;
    for node in available_nodes {
        // Filter: default selects non-root tasks; -T selects only root tasks
        if node.id.task.as_str() == requested && node.id.is_root() == top_level {
            requested_ids.insert(node.id.clone());
            matched = true;
        }
    }
    matched
}
```

`-T`/`--top-level` flag applies to all positional task args — no mixed package+root selection in single invocation.

## Why This Works

- **Root package wiring**: Matching `PackageNode.path == workspace_root` identifies the actual root package. `YarnWorkspace::discover` already includes it when `package.json` has a `name` field.

- **Engine guard**: `skip_global_tasks_for_root` uses `Option<&PackageName>` comparison, which is `Some` only when root package exists and has a name. Nameless root leaves `root_package = None`, preserving existing behavior.

- **CLI filter**: `TaskId::is_root()` checks the synthetic `//root` sentinel, controlled by `-T` flag. Separately, actual root package tasks (`my-repo#build`) never created because global expansion skipped.

- **Separation of concerns**: Synthetic `//root` (for `#task` config keys) vs actual root package (discovered entity) are handled by completely different code paths.

## Prevention Strategies

**Test Cases:**

- Integration tests encoding OLD behavior must be updated: root `#task` tests now require `-T` flag
- Global expansion count decreased when root excluded: `Done: 3 tasks` → `Done: 2 tasks`
- Distinguish "test encodes old behavior" from "real regression" when behavior changes

**Best Practices:**

- When adding optional graph metadata (`with_root_package`), ensure production code wires it before relying on it
- Two-phase test updates: first identify tests encoding old behavior, then update intentionally
- Close the gap between "method exists" and "method called in production"

**Code Review Checklist:**

- [ ] Does `prepare_workspace` wire optional graph metadata?
- [ ] Do integration tests still encode old behavior after semantic changes?
- [ ] Are synthetic sentinels (e.g., `//root`) handled separately from real entities?

## Related Issues

- **Prior Art:** [task-graph-scoped-keys-and-dependency-resolution-2026-06-10.md](task-graph-scoped-keys-and-dependency-resolution-2026-06-10.md) — Introduced `PipelineKey` parsing and scope-local deps
- **GitHub:** [#69](https://github.com/dobesv/luchta/issues/69) — Stop running top-level tasks by default
