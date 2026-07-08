---
title: "Watch mode did not re-run tasks that depend on a fixed/re-run upstream task"
date: 2026-07-07
category: logic-errors
problem_type: logic_error
component: luchta-cli/watch
root_cause: "Watch driver expanded only upstream dependencies of affected packages, not downstream dependents; failed-task downstreams were marked Skipped and never scheduled"
resolution_type: code_fix
severity: high
tags:
  - watch-mode
  - task-graph
  - transitive-dependents
  - PackageGraph
  - downstream-propagation
  - affected-packages
plan_ref: watch-dep-tasks-not-running
issue: "#186"
---

## Problem

Watch mode did not re-run tasks in packages that *depend* on an upstream package after that upstream task was fixed or re-run. When file X in package P changed, watch mode would run P's tasks but not tasks in packages transitively depending on P.

## Symptoms

- Case (a): Upstream task fails → fix the failure → watch mode re-runs upstream task → dependent downstream tasks stay idle (should run)
- Case (b): Upstream task succeeds → file changes in upstream → watch re-runs upstream task → dependent downstream tasks stay idle (should run)
- User observable: downstream tasks never scheduled after upstream fix/change in watch mode
- One-shot `luchta run --since <ref>` correctly included dependents; watch mode did not

## Root Cause

In `crates/luchta-cli/src/watch/driver.rs`, `run_one_iteration`:

1. Computed `affected: HashSet<PackageName>` from changed files
2. Passed affected set to cycle request via task-graph selection
3. Task-graph selection (`collect_requested_subgraph` → `dependencies_of`) expands **upstream** dependencies only (prerequisites)
4. **Never expanded to downstream dependents** — the symmetric step that `run --since` already had

Additionally: prior behavior in watch mode marked downstream tasks of a failed task as `Skipped` by the walker, so they were never registered/ran even if later eligible.

Key asymmetry:

```
# File X in package P changes

# --since (correct):
P's tasks + all packages transitively depending on P

# watch (buggy):
P's tasks only (missing downstream dependents)
```

`since.rs::affected_packages_from_paths` called `PackageGraph::transitive_dependents_of`. Watch driver never did.

## Solution

After collecting directly-affected packages (post lockfile handling, before building the cycle request), expand the `affected` set to include transitive dependents:

```rust
// driver.rs, run_one_iteration, after affected set computed

fn expand_affected_with_dependents(
    graph: &PackageGraph,
    affected: &mut HashSet<PackageName>,
) {
    let seeds: Vec<PackageName> = affected.iter().cloned().collect();
    match graph.transitive_dependents_of(&seeds) {
        Ok(dependents) => {
            affected.extend(dependents);
        }
        Err(e) => {
            warn!(?e, "failed to expand affected with dependents, using direct set");
            // fallback: use direct affected set only
        }
    }
}
```

Call site:

```rust
// After lockfile handling sets affected, before cycle request
expand_affected_with_dependents(&self.package_graph, &mut affected);
```

This mirrors `since.rs::affected_packages_from_paths` pattern:

```rust
// since.rs (existing)
let affected = discover_affected_packages(&paths, &package_graph)?;
let dependents = graph.transitive_dependents_of(&affected)?;
affected.extend(dependents);
```

Now watch and `--since` share symmetric behavior.

## Why This Works

`PackageGraph::transitive_dependents_of(seeds)` performs BFS over `Direction::Incoming` edges (each edge points dependent → dependency, so incoming neighbors are dependents). It:

- Includes seed packages in result
- Handles cycles safely
- Returns empty set for unknown seeds (non-blocking)

By expanding `affected` to include transitively downstream packages, the subsequent task-graph selection picks up their tasks. When upstream package P changes:

- Direct change: P is in `affected`
- Downstream: all packages depending on P transitively are added to `affected`
- Task graph: finds tasks for both direct and dependent packages

This restores the invariant: changes in P trigger tasks in P AND all downstream dependents.

## Tests

**Unit test:**

```rust
#[test]
fn expand_affected_with_dependents_includes_downstream_packages() {
    // Graph: app → api (app depends on api)
    let graph = test_package_graph();
    let mut affected = hashset!["api".into()];
    expand_affected_with_dependents(&graph, &mut affected);
    assert!(affected.contains("app"));
}
```

**E2E tests (driver_e2e_tests.rs):**

- `failed_upstream_fix_reruns_dependent_task` — case (a): upstream fails, fix pushed, both upstream and downstream tasks run
- `rerun_upstream_change_reruns_dependent_task` — case (b): upstream change triggers both upstream and downstream tasks

Fixture (driver_e2e_support.rs): two-package workspace (`app` depends on `api`).

**Verification:**

```bash
cargo fmt --check
cargo clippy --all-targets
cargo nextest run --stress-count=5  # 1030 tests, all green
```

## Prevention Strategies

**Code review checklist:**

- [ ] When computing "affected" packages from file changes, always expand transitive dependents
- [ ] Verify watch mode and `--since` share symmetric downstream-propagation logic
- [ ] Cross-package dependency changes require E2E tests with multi-package fixtures

**Best practices:**

- Use `PackageGraph::transitive_dependents_of` for downstream expansion (proven API from `--since`)
- Prefer warn-and-fallback over panic when graph queries fail (structural churn during watch)
- Keep affected-package computation consistent across one-shot and watch modes

## Related Issues

- **GitHub:** [#186](https://github.com/dobesv/luchta/issues/186) — watch dependent tasks not re-running
- **Prior Art:** [logic-errors/since-filter-selection-and-gix-change-detection-2026-06-18.md](./since-filter-selection-and-gix-change-detection-2026-06-18.md) — `--since` filter correctly used `transitive_dependents_of`
- **Related:** [logic-errors/watch-failed-cycle-worker-manager-poisoning-2026-07-06.md](./watch-failed-cycle-worker-manager-poisoning-2026-07-06.md) — watch state registration after failed cycles (independent but related symptom)

## Known Follow-up

`transitive_dependents_of` silently skips seed packages not present in the graph (possible during structural churn). This is a narrow race worth a future WARN log. Non-blocking — does not affect correctness.
