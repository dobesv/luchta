---
title: "Luchta task-graph construction: package-scoped keys, dependency resolution, and cycle debugging"
date: 2026-06-10
category: logic-errors
problem_type: logic_error
component: luchta-engine
root_cause: "Config key scoping misinterpreted; SamePackage dependency resolution crossed package boundaries"
resolution_type: code_fix
severity: critical
tags:
  - task-graph
  - dependency-resolution
  - monorepo
  - turbo/lage-semantics
  - cycle-detection
  - petgraph
  - scoped-tasks
plan_ref: luchta-pkg-scoped-tasks
---

## Problem

Task graph construction misinterpreted config key scoping (`task` vs `pkg#task` vs `#task`), materializing package-scoped overrides as per-package nodes with wrong package identities. A regression in `SamePackage` dependency resolution caused cross-package edges, producing 400-node cycles in large monorepos.

## Symptoms

```text
failed to build task graph: unknown task '@otherpkg#@scoped/pkg#task'
task graph cycle detected (non-deterministic node)
```

- Bogus node IDs like `@otherpkg#@scoped/pkg#task` appearing in graph
- Cycles detected during toposort; node named in error non-deterministic (HashMap iteration order)
- `luchta check` passed but `luchta run build` failed with cycle error
- SamePackage dependencies resolved to tasks in wrong packages

## Investigation Steps

1. Traced node creation to `parse_pipeline_entries` — every raw key treated as bare task name, ignoring `pkg#` prefix and `#` root prefix.
2. Identified bogus node `@otherpkg#@scoped/pkg#task` from a `pkg#task` config entry where package name contained `@` scope.
3. Found dependency resolution collecting ALL packages declaring a task name, then filtering by existence rather than package identity.
4. Reproduced 400-node cycle: `X#build:types` SamePackage dep `build:generate-json-schema` (declared only as `pkg#build:generate-json-schema` for 3 packages) resolved to ALL 3 packages' tasks, each depending on `^build:types`, forming cycle.
5. Used `petgraph::algo::kosaraju_scc` to dump strongly-connected components and in-cycle out-edges for debugging.

## Root Cause

**Config key scoping:** Original builder treated every `tasks` map key as a per-package task name. Convention (turbo/lage):

- `task` = GLOBAL (materialize one node per package)
- `pkg#task` = PACKAGE-SCOPED override (only that package; shadows global `task` for that package)
- `#task` = ROOT singleton (one node under synthetic `//root` package)

Keys were not parsed into structured `PipelineKey` variants; instead, raw strings propagated into node IDs.

**SamePackage regression:** `SamePackage(task)` resolved by collecting ALL packages declaring that task name, filtering only by existence, not package identity. Edges pointed to tasks in unrelated packages, creating cycles when those tasks had upstream dependencies.

**Root task handling:** Root `#task` keys created per-package nodes rather than single nodes under `//root`.

## Solution

### 1. PipelineKey Parsing

Parse each config key into `PipelineKey` enum:

```rust
enum PipelineKey {
    Global { task: TaskName },
    Package { package: PackageName, task: TaskName },
    Root { task: TaskName },
}

fn parse_pipeline_key(key: &str, known_packages: &HashSet<PackageName>) -> Option<PipelineKey> {
    if let Some(task) = key.strip_prefix('#') {
        Some(PipelineKey::Root { task: TaskName::from(task) })
    } else if let Some((pkg, task)) = key.split_once('#') {
        let pkg_name = PackageName::from(pkg);
        if known_packages.contains(&pkg_name) {
            Some(PipelineKey::Package { package: pkg_name, task: TaskName::from(task) })
        } else {
            None // DROP unknown-package scoped keys; don't promote to Global
        }
    } else {
        Some(PipelineKey::Global { task: TaskName::from(key) })
    }
}
```

### 2. ResolvedPipeline Construction

Build per-package resolved table with shadowing:

```rust
struct ResolvedPipeline {
    tasks_by_package: HashMap<PackageName, HashMap<TaskName, TaskDefinition>>,
    root_tasks: HashMap<TaskName, TaskDefinition>,
}

fn build(pipeline: &HashMap<String, TaskDefinition>, packages: &HashSet<PackageName>) -> ResolvedPipeline {
    let mut resolved = ResolvedPipeline::new();
    
    // First: global tasks apply to all packages
    for (key, def) in pipeline {
        if let Some(PipelineKey::Global { task }) = parse_pipeline_key(key, packages) {
            for pkg in packages {
                resolved.tasks_by_package
                    .entry(pkg.clone())
                    .or_default()
                    .insert(task.clone(), def.clone());
            }
        }
    }
    
    // Second: package-scoped override SHADOWS global (replace, not merge)
    for (key, def) in pipeline {
        if let Some(PipelineKey::Package { package, task }) = parse_pipeline_key(key, packages) {
            resolved.tasks_by_package
                .entry(package)
                .or_default()
                .insert(task, def.clone());
        }
    }
    
    // Third: root tasks are singletons
    for (key, def) in pipeline {
        if let Some(PipelineKey::Root { task }) = parse_pipeline_key(key, packages) {
            resolved.root_tasks.insert(task, def.clone());
        }
    }
    
    resolved
}
```

### 3. Dependency Resolution (Package-Scoped)

```rust
fn expand_dependency(dep: &DependsOn, source: &TaskId, resolved: &ResolvedPipeline, pkg_graph: &PackageGraph) -> Vec<TaskId> {
    match dep {
        DependsOn::SamePackage(task) => {
            // EXACTLY source_package#task (skip if undefined)
            if resolved.has_task(&source.package, task) {
                vec![TaskId { package: source.package.clone(), task: task.clone() }]
            } else {
                vec![]
            }
        }
        DependsOn::Specific(pkg, task) => {
            vec![TaskId { package: pkg.clone(), task: task.clone() }]
        }
        DependsOn::Root(task) => {
            vec![root_task_id(task)]
        }
        DependsOn::DirectUpstream(task) => {
            // Filter by ACTUAL direct upstream packages
            pkg_graph.direct_upstreams(&source.package)
                .filter(|pkg| resolved.has_task(pkg, task))
                .map(|pkg| TaskId { package: pkg, task: task.clone() })
                .collect()
        }
        DependsOn::TransitiveUpstream(task) => {
            // Filter by ACTUAL transitive upstream packages
            pkg_graph.transitive_upstreams(&source.package)
                .filter(|pkg| resolved.has_task(pkg, task))
                .map(|pkg| TaskId { package: pkg, task: task.clone() })
                .collect()
        }
    }
}
```

### 4. Cycle Debugging Utility

```rust
fn debug_cycle_if_env(graph: &DiGraph<TaskId, ()>) {
    if std::env::var("LUCHTA_DEBUG_CYCLE").is_ok() {
        let sccs = petgraph::algo::kosaraju_scc(graph);
        for scc in sccs.iter().filter(|scc| scc.len() > 1) {
            eprintln!("Cycle detected ({} nodes):", scc.len());
            for idx in scc {
                let node = &graph[*idx];
                let out_edges: Vec<_> = graph.edges(*idx)
                    .filter(|e| scc.contains(&e.target()))
                    .map(|e| format!("  -> {}", graph[e.target()]))
                    .collect();
                eprintln!("  {} -> edges in cycle:\n{}", node, out_edges.join("\n"));
            }
        }
    }
}
```

### 5. Execution Model

All tasks must be DECLARED in config. No package.json script fallback for task creation.

```rust
// Worker present => runs (command = explicit or bare task name)
// No worker + no command => no-op ordering node
// No worker + HAS command => CONFIG ERROR

fn resolve_command(task_def: Option<&TaskDefinition>) -> Option<Result<String, CommandError>> {
    match task_def {
        None => None, // no-op
        Some(def) if def.worker.is_some() => {
            Some(Ok(def.command.as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .unwrap_or(&def.task_name)
                .to_string()))
        }
        Some(def) if def.command.is_some() => {
            Some(Err(CommandError::CommandWithoutWorker))
        }
        Some(_) => None, // no-op
    }
}
```

### 6. Check vs Run Parity

- `luchta run`: tolerant (silently skips edges to undeclared tasks)
- `luchta check`: strict (reports dead refs, command-without-worker, unknown-worker)
- Both: validate only DECLARED config tasks (scripts don't count)
- A dep resolving for SOME packages but not others is NOT flagged

## Why This Works

- **PipelineKey parsing**: Correctly separates global, package-scoped, and root tasks before materialization
- **Shadowing**: Package-scoped `pkg#task` replaces (not merges with) global `task` for that package only
- **SamePackage resolution**: Edges point ONLY to the source package's task, never cross-package
- **Upstream resolution**: Candidates filtered by actual package-graph membership, not existence-anywhere
- **Cycle debugging**: `kosaraju_scc` finds actual SCC; printing in-cycle edges reveals the cycle chain
- **Execution model enforcer**: Config error for command-without-worker caught by check; run fails only that task if selected, others proceed

## Prevention Strategies

**Test Cases:**

```rust
#[test]
fn same_package_dependency_does_not_resolve_across_packages() {
    // Chain a -> b -> c
    // b#gen declared only for b
    // Assert a#build and c#build get NO edge to b#gen
}

#[test]
fn scoped_override_shadows_global_for_that_package_only() {
    // Global build = "yarn build"
    // pkg-b#build = "yarn custom-build"
    // Assert pkg-a#build uses global, pkg-b#build uses override
}

#[test]
fn unknown_package_scoped_key_dropped_not_promoted() {
    // Config has non-existent-pkg#task
    // Assert NO nodes created for any package with that task name
}
```

**Best Practices:**

- Always parse pipeline keys into structured enum before processing
- SamePackage MUST check explicit package identity, not existence-anywhere
- Use `kosaraju_scc` for cycle debugging; HashMap iteration order is non-deterministic
- Gate debug utilities behind env vars (`LUCHTA_DEBUG_CYCLE`) for production safety
- Separate tolerant expansion (run) from strict validation (check)

**Code Review Checklist:**

- [ ] SamePackage dependency resolves to exactly `source.package#task`?
- [ ] DirectUpstream/TransitiveUpstream filtered by actual package-graph membership?
- [ ] Unknown-package `pkg#task` keys dropped (not promoted to Global)?
- [ ] Root `#task` creates single node under `//root`?
- [ ] Tests cover cross-package edge prevention?

## Related Issues

- **Plan:** [luchta-pkg-scoped-tasks](../../plans/luchta-pkg-scoped-tasks) — Fix package-qualified config keys treated as global tasks
- **Related Solution:** [yarn-workspace-command-override-2026-06-10.md](yarn-workspace-command-override-2026-06-10.md) — Worker command resolution
