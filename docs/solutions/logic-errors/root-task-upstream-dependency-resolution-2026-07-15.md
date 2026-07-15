---
title: "Root-task ^/^^ upstream dependency resolution against workspace-root package.json"
date: 2026-07-15
category: logic-errors
problem_type: logic_error
component: luchta-engine (task_graph.rs, input_expansion.rs)
root_cause: "Synthetic //root package name absent from PackageGraph; upstream helpers silently returned empty"
resolution_type: code_fix
severity: high
tags:
  - task-graph
  - root-tasks
  - upstream-dependencies
  - package-graph
  - input-expansion
plan_ref: root-upstream-deps-51
---

## Problem

Root tasks (package `//root`) with `^task` or `^^task` upstream dependencies resolved to NOTHING instead of the workspace-root package.json's actual dependencies. The synthetic `//root` package name used for task identity does not exist in `PackageGraph`, so graph lookups failed silently.

## Symptoms

```yaml
# root package.json (workspace root named "repo" in PackageGraph)
dependencies:
  lib: "workspace:*"

# root turbo.json
tasks:
  build:
    inputs: ["^build"]  # should resolve lib#build, but resolved to nothing
```

- Root `^build` silently expanded to empty set
- Root `^^build` silently expanded to empty set
- No error raised; tasks simply missing upstream inputs
- Cache invalidation missed upstream changes

## Investigation Steps

1. Traced `is_direct_upstream`/`is_transitive_upstream` in task_graph.rs: both called `dependencies_of(source_pkg)` where `source_pkg` was `//root` for root tasks.

2. `PackageGraph::dependencies_of("//root")` returned `Err(UnknownPackage("//root"))` — synthetic root never added to graph, only discovered packages under real names.

3. Found `.unwrap_or(false)` in upstream predicates and early returns in helper functions swallowed errors silently.

4. Confirmed `PackageGraph::with_root_package(realName)` and `root_package()` existed but were not wired for upstream resolution.

5. Early fix attempt set `from_resolved_pipeline`'s `root_package` to the real name — broke `DependsOn::Root` matching (see pitfall #1 below).

## Root Cause

Two identities conflated:

**Synthetic `//root`**: Used for task-id identity (`//root#build`), `DependsOn::Root` (`#task`) matching, and dependency-is-root guards. Must remain `//root`.

**Real root package name**: The actual package name from `PackageGraph::root_package()` (e.g., `repo`). Used for graph traversal of `^`/`^^` upstreams.

Upstream helpers passed task's `package` field (`//root`) directly to graph lookups. Graph only contains real package names. Result: `UnknownPackage` errors, swallowed by error-handling, producing empty upstream sets.

## Solution

Translate `//root` → real root name **locally** at each graph-traversal call site. Keep synthetic `//root` for all other purposes.

### task_graph.rs

```rust
// Helper (DRY): translate synthetic root to real root for upstream lookups
fn upstream_source_package(source_pkg: &str, graph: &PackageGraph) -> String {
    if source_pkg == root_package_name() {
        graph.root_package()
            .map(|s| s.to_string())
            .unwrap_or_else(|| source_pkg.to_string())
    } else {
        source_pkg.to_string()
    }
}

// In is_direct_upstream / is_transitive_upstream:
let lookup_pkg = upstream_source_package(&task_id.package, package_graph);
let deps = package_graph.dependencies_of(&lookup_pkg);
// ... use deps for upstream resolution
```

- `from_resolved_pipeline`: keep `root_package = root_package_name()` (synthetic) for `DependsOn::Root` filter
- Remove `is_root()` early return from `transitive_upstream_packages` — was hiding the problem

### input_expansion.rs

```rust
// In expand_input_patterns, only for ^/^^ patterns:
let upstream_source = if source_pkg == root_package_name() {
    graph.root_package()
        .map(|s| s.to_string())
        .unwrap_or_else(|| source_pkg.to_string())
} else {
    source_pkg.clone()
};
// Use upstream_source for graph traversal; preserve source_pkg for other context
```

- Safe fallback: if graph has no real-root tag, treat root upstreams as empty (no error)
- Remove synthetic-root short-circuit from `direct_upstream_packages`

## Why This Works

Translation happens **only** at graph-traversal call sites. Synthetic `//root` remains the canonical task-id package for:

- Task identity (`//root#build`)
- `DependsOn::Root` matching (`#task` filters compare against `//root`)
- Dependency-is-root guards

Graph receives real package names for lookups, returning correct workspace dependency edges.

## Key Non-Obvious Pitfalls

### 1. REGRESSION TRAP: Changing root_package used for matching

**Mistake**: Set `from_resolved_pipeline`'s `root_package` variable to real root name.

**Consequence**: `DependsOn::Root` filter (`&task_id.package == root_package`) broke — task ids use `//root`, filter compared against real name (`repo`). Result: downstream `app#build` ran BEFORE root `#build` produced its output.

**Lesson**: Variables used for `DependsOn::Root` matching and dependency-is-root guards MUST stay synthetic `//root`. Only SOURCE translation uses the real name.

### 2. SHARED-HELPER TRAP: Removing short-circuit without auditing callers

**Mistake**: Removed `is_root()` early return from `transitive_upstream_packages` (needed so translated callers work) without auditing input_expansion.rs.

**Consequence**: `input_expansion.rs` imports same function, still passed raw `//root` → began erroring with `UnknownPackage("//root")`.

**Lesson**: When removing a short-circuit in a shared function, audit ALL callers and translate at every call site.

### 3. TEST-TOOLING TRAP: cargo test vs cargo-nextest

**Mistake**: Used plain `cargo test` for verification.

**Consequence**: Env/cwd/process-tree (RSS) tests and cache-e2e tests flaked under parallelism, producing phantom "failures" (progress/dispatch/watch/cache-nonce). Wasted debugging cycles on non-issues.

**Lesson**: This project uses cargo-nextest (per AGENTS.md). Plain `cargo test` lacks per-test process isolation.

**Correct verification**:
```bash
cargo nextest run --workspace              # primary
cargo nextest run --workspace --stress-count=5  # flake detection
cargo test --test-threads=1               # fallback only
```

Distinguish real regressions from flakes by running specific test in isolation and/or via nextest.

### 4. VERIFICATION: Git stash for regression confirmation

When a suspected-regression test might be pre-existing:

```bash
git stash                  # save changes
cargo nextest run <test>   # check baseline
git stash pop              # restore changes
cargo nextest run <test>   # verify fix
```

Proved `cache_root_task_output_change_reruns_downstream` was a real regression (failed on clean baseline), not pre-existing.

## Prevention Strategies

**Code Review Checklist**:
- [ ] Does this change affect both synthetic `//root` and real root name?
- [ ] Are there shared helper functions with multiple callers?
- [ ] Have all callers been audited for translation?

**Test Patterns**:
- Root `^task` resolves to direct workspace dependency
- Root `^^task` resolves transitively through root→pkg→dep chain
- Root upstreams graceful fallback when graph lacks real-root tag
- `DependsOn::Root` ordering preserved after root-package changes

**Verification**:
- Always use `cargo nextest run --workspace`
- Run `--stress-count=5` for flake detection
- Isolate suspected regressions with git stash comparison

## Related Issues

- **Issue**: [#51](https://github.com/dobesv/luchta/issues/51) — Root-task ^/^^ upstream deps resolve to nothing
- **Prior Art**: [root-task-exclusion-and-global-expansion-skip-2026-06-15.md](root-task-exclusion-and-global-expansion-skip-2026-06-15.md) — Established synthetic `//root` vs real root package distinction
