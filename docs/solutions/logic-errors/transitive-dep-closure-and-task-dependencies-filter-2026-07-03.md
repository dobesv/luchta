---
title: "Transitive lockfile dependency closure and per-task dependencies filter"
date: 2026-07-03
category: logic-errors
problem_type: logic_error
component: luchta-lockfiles
root_cause: "Deep transitive resolved-version bumps were not detected (cache poisoning risk) and users lacked control over which dependencies affect which tasks"
resolution_type: code_fix
severity: high
tags:
  - lockfile
  - transitive-dependencies
  - cache-invalidation
  - filter
  - yarn
  - bfs
  - cycle-handling
plan_ref: luchta-transitive-deps
issue: ["#89", "#90"]
---

## Problem

Prior to this change, Luchta's cache invalidation only tracked direct dependencies plus one level of depth from the lockfile. This created a "transitive blindness" gap where a deep dependency's resolved version could change (e.g., a security patch in a nested sub-dependency) without busting the cache of tasks that ultimately depend on it. This posed a significant cache poisoning risk.

Additionally, all package dependencies were treated as significant for all tasks in that package. Users had no way to specify that a particular task (e.g., a linter) only cared about a subset of dependencies, leading to unnecessary cache misses when unrelated dependencies were updated.

## Symptoms

- **Transitive blindness**: `yarn.lock` changes to deep transitive dependencies did not trigger task reruns when the direct dependency specifier (in `package.json`) remained unchanged.
- **Over-invalidation**: Tasks reran when unrelated dependencies changed, reducing cache hit rates.
- **Flipped watch test**: Existing test `deep_transitive_resolved_version_change_is_not_detected_matches_cache_dep_hash` asserted non-detection — had to be flipped to assert detection after fix.

## Investigation Steps

Started by reviewing existing `Lockfile` trait and `gather_pkg_dep_pairs` implementation. Found that `all_dependencies` returns only immediate children. Lockfile parsers (Yarn v1 and Yarn Berry) are round-trippable — dependency values can be re-resolved via `resolve_package`. Designed and implemented iterative BFS for transitive closure with `visited: HashSet` for cycle safety.

For the filter feature, analyzed `InputPattern` grammar (already used for `inputs`) and mapped its variants to dependency-root selection contexts. Key decision: Interpretation A — filter selects closure roots, each matched root contributes its FULL transitive closure.

## Root Cause

**Transitive depth limitation**: `collect_dep_pairs_for_package` walked only direct deps + immediate children. Full transitive closure was never computed, so deep resolved-version changes were invisible to cache-hash.

**Filter semantics**: No mechanism existed to narrow which dependency roots fed the dep hash. Watch-mode invalidation was the same conservative superset for all tasks.

## Solution

### Transitive Closure Detection (#89)

Added `Lockfile::transitive_dependencies` trait method returning `BTreeSet<(String, String)>`:

```rust
fn transitive_dependencies(
    &self,
    key: &str,
) -> Result<BTreeSet<(String, String)>, LockfileError> {
    let mut dependencies = BTreeSet::new();
    let mut visited = HashSet::new();
    let mut pending = VecDeque::new();

    // Seed the queue with the immediate deps of `key`, as (name, range) pairs.
    pending.extend(self.all_dependencies(key)?);

    while let Some((dependency_name, dependency_range)) = pending.pop_front() {
        // `resolve_package` returns `Result<Option<Package>, _>`; skip deps
        // that don't resolve in the lockfile (silent).
        let Some(package) = self.resolve_package("", &dependency_name, &dependency_range)? else {
            continue;
        };

        dependencies.insert((dependency_name, package.version.clone()));

        // Break cycles via `visited` on resolved keys; only expand each key once.
        if visited.insert(package.key.clone()) {
            pending.extend(self.all_dependencies(&package.key)?);
        }
    }

    Ok(dependencies)
}
```

**Critical design choices:**

1. **Iterative BFS, not recursive** — avoids stack overflow on deep/cyclic graphs.
2. **Silent cycle handling** — `visited` set breaks cycles; no error/warn because users cannot fix peer-dep cycles.
3. **`BTreeSet<(String, String)>` not `BTreeMap<String, String>`** — real lockfiles contain multiple versions of the same package name in one closure; a map would clobber all but one.

### Per-Task `dependencies` Filter (#90)

Added `dependencies: Vec<String>` to `TaskDefinition` with default `["**/*"]`:

```rust
#[serde(default = "default_dependencies")]
pub dependencies: Vec<String>,

fn default_dependencies() -> Vec<String> { vec!["**/*".to_string()] }
```

Pattern resolution reuses `InputPattern` enum:

- `SamePackage(glob)` — own package's dependencies
- `DirectUpstream(glob)` (`^`) — direct upstream packages' dependencies
- `TransitiveUpstream(glob)` (`^^`) — transitive upstream packages' dependencies
- `Specific(pkg, glob)` (`pkg#`) — specific package's dependencies
- `Root(glob)` (`#`) — workspace root's dependencies

**Interpretation A:** Filter selects closure ROOTS; each matched root contributes its FULL transitive closure. Never post-filter the flattened pairs by name.

**Example:** If `dependencies: ["left-pad"]` and `left-pad` depends on `repeat-string`, then:
- `left-pad@1.0.0` → `repeat-string@3.0.0` both in hash
- Bumping `repeat-string` to `3.0.1` busts cache
- Bumping an unrelated dependency like `chalk` does NOT bust cache

### Cache/Watch Consistency Design

`gather_pkg_dep_pairs` is per-PACKAGE and shared by both paths:

- **Cache path**: Uses `gather_pkg_dep_pairs_filtered` with task's `dependencies` filter. Watches `pkg_dep_hash` against `CurrentState`.
- **Watch path**: Uses unfiltered `gather_pkg_dep_pairs`. Conservative superset — over-wakes, never misses a rebuild.

This is SAFE: watch over-wakes, but cache under-invalidates only within the user's explicit filter. A missed rebuild is impossible.

### Worker Override

Workers can replace the static filter via `TaskModification`:

```rust
pub struct TaskModification {
    // ... other fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<String>>,
}
```

Full REPLACE semantics — `None` leaves static filter unchanged; `Some(list)` completely replaces it.

## Bugs Caught During Review

### Bug 1: Multi-Version Clobbering (Aristarchus)

**Problem:** Initial implementation returned `BTreeMap<String, String>`, clobbering multiple versions of the same package name.

**Impact:** Cache poisoning — changes to shadowed versions wouldn't bust cache.

**Fix:** Changed return type to `BTreeSet<(String, String)>`. Added regression test `transitive_dependencies_retains_multiple_versions_of_same_name`.

### Bug 2: `^^` Self-Inclusion (Aristarchus)

**Problem:** `PackageGraph::transitive_dependencies_of` includes the seed node in results. `^^glob` became a superset of `SamePackage(glob)`.

**Impact:** Inconsistent semantics — `^^` for dependencies behaved differently than `^^` for file inputs.

**Fix:** Filter out `package.name` in the `TransitiveUpstream` arm:
```rust
.filter(|upstream_name| upstream_name != &package.name)
```

### Bug 3: Inheritance Drop (Argus)

**Problem:** `select_dependency_roots` returned `HashSet<String>` of names only. Roots selected from upstream packages' `package.json` lacked version and origin path needed for lockfile resolution.

**Impact:** Inherited roots (`^`/`^^`/`pkg#`/`#`) were silently dropped.

**Fix:** Return `BTreeSet<SelectedDependencyRoot { name, version, origin_package_path }>`. Each root carries its own resolution context.

### Bug 4: Post-Filter on Flattened Closure (Design Review)

**Problem:** Almost filtered the flattened closure pairs by name, which would drop transitive members whose names don't match the glob.

**Impact:** Would violate Interpretation A — a matched root's closure would be incomplete.

**Fix:** Filter at ROOT level only. Each selected root contributes its full closure untouched.

## Why This Works

1. **Deterministic hashing**: `BTreeSet<(String, String)>` ensures stable iteration order for BLAKE3 hashing.
2. **Single source of truth**: `gather_pkg_dep_pairs` remains the canonical dependency-pair collector for cache and watch.
3. **Conservative watch**: Watch over-wakes safely; cache decides whether to skip based on filtered hash.
4. **Pattern reuse**: `InputPattern` grammar already understood by users from `inputs` field.
5. **BFS cycle safety**: Iterative traversal with `visited` set; no recursion limits.

## Prevention Strategies

### Test Cases

- **Per-parser closure/cycle/determinism tests** — verify BFS correctness for Yarn v1 and Yarn Berry
- **Multi-version retention test** — `transitive_dependencies_retains_multiple_versions_of_same_name`
- **Flipped watch test** — `deep_transitive_resolved_version_change_busts_cache`
- **E2E transitive bump tests** — `cache_yarn_v1_transitive_dep_bump_reruns`, `cache_yarn_berry_transitive_dep_bump_reruns`
- **Filter narrowing test** — bump excluded dep → cache hit; bump included dep → rerun
- **`^^` exclusion test** — `select_dependency_roots_transitive_upstream_excludes_source_package`
- **Worker override tests** — replace vs. keep static filter

### Best Practices

- **Always use `BTreeSet<(String, String)>` for dependency pairs** — maps clobber multi-version closures.
- **Filter roots, not closures** — Interpretation A: select roots, include full closure per root.
- **Carry origin context** — selected roots from other packages must carry `(name, version, origin_package_path)`.
- **Exclude self for `^^`** — `transitive_dependencies_of` includes seed; filter it out for dependency-root selection.
- **Keep watch conservative** — watch over-wakes; cache applies precise filter.
- **Iterative BFS for graph traversal** — avoid stack overflow on deep/cyclic structures.

### Code Review Checklist

- [ ] Does `transitive_dependencies` return a `BTreeSet` of pairs (not a map)?
- [ ] Does filter selection happen at the ROOT level?
- [ ] Does `select_dependency_roots` return roots with origin context?
- [ ] Does `^^` exclude the source package (not just rely on graph semantics)?
- [ ] Does watch path use unfiltered `gather_pkg_dep_pairs`?
- [ ] Does cycle handling use iterative BFS with `visited` set?

## Known Gaps (Deferred)

1. **peerDependencies omitted** — `collect_external_dependencies` chains `dependencies/devDependencies/optionalDependencies` but not `peerDependencies`. Changes to resolved peer deps may not bust cache (notable for Yarn Berry).
2. **DashMap memoization** — Phase 3 deferred. BLAKE3 hashing is microsecond-scale; parsing dominates.
3. **Path-centric InputPattern docs** — docstrings say "path" but type now reused for dependency names.

## Related Issues

- **GitHub:** [dobesv/luchta#89](https://github.com/dobesv/luchta/issues/89) — Full transitive lockfile dependency detection
- **GitHub:** [dobesv/luchta#90](https://github.com/dobesv/luchta/issues/90) — Per-task `dependencies` filter
- **Prior Art:** [logic-errors/lockfile-invalidation-selective-rebuild-2026-07-02.md](./lockfile-invalidation-selective-rebuild-2026-07-02.md) — Initial lockfile invalidation (direct-only), established `gather_pkg_dep_pairs` as single source of truth.

## Verification

- `cargo nextest run --workspace`: 965 passed
- Clippy clean
- All transitive, filter, and worker-override tests pass
