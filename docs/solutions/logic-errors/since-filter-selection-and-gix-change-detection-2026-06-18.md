---
title: "--since git-ref filter: selection-matrix pitfalls and gix 0.73 change detection"
date: 2026-06-18
category: logic-errors
problem_type: logic_error
component: luchta-cli, luchta-workspace
root_cause: "Since-filter must intersect every non-root selection arm and must not map repo-root files to the root package; gix 0.73 tree-diff/status API is non-obvious"
resolution_type: feature
severity: medium
tags:
  - task-selection
  - selection-matrix
  - goal-not-filter
  - gix
  - tree-diff
  - git-status
  - package-graph
  - transitive-dependents
plan_ref: luchta-since-filter
issue: "#15"
---

## Problem

Add `luchta run --since <git-ref>`: restrict goal-task selection to packages
changed since a ref — committed (`ref..HEAD`) OR staged OR unstaged OR
untracked-not-gitignored, within the package folder — plus their transitive
dependents, intersected with existing `-p`/`-T`/task-name filters. No affected
packages → exit 0 (no-op). Use `gix` (already a dependency), not `git2`, not
shelling out.

The feature is small in surface area but has several non-obvious traps that each
produced a wrong-but-green implementation before being caught.

## Solution Overview

- `PackageGraph::transitive_dependents_of(seeds)` — BFS over
  `Direction::Incoming` (edges point dependent → dependency, so incoming
  neighbors are dependents). Includes the seeds, cycle-safe, skips unknown seeds.
- `since.rs`: `changed_paths_since()` (gix tree-diff + working status, union),
  `discover_repo_root()`, `affected_packages()` (map changed paths → packages,
  union transitive dependents), `SinceError` (thiserror + miette `Diagnostic`).
- `run.rs`: `SelectionCriteria.since_affected`, applied in `package_matches`;
  empty affected set → no-op via a shared `resolve_since_selection` helper used
  by both `run_tasks` and `dry_run_tasks`. `expand_with_dependencies` untouched.

## Key Learnings (the compounding value)

### 1. gix 0.73 change-detection API (verified working)

Tree diff `ref..HEAD`:

```rust
let base = repo.rev_parse_single(since_ref)?.object()?.peel_to_tree()?;
let head = repo.rev_parse_single("HEAD")?.object()?.peel_to_tree()?;
let mut changes = base.changes()?;
changes.options(|o| { o.track_rewrites(None); });   // see pitfall below
changes.for_each_to_obtain_tree(&head, |change| -> Result<_, std::convert::Infallible> {
    // collect change.location() (repo-relative &BStr) for Addition/Deletion/Modification
    Ok(gix::object::tree::diff::Action::Continue)
})?;
```

- **Pitfall**: a direct `changes.track_rewrites(None)` does **not** exist on the
  0.73 diff platform. You must go through `changes.options(|o| o.track_rewrites(None))`.
- Convert a repo-relative `&BStr` location to a path with
  `gix::path::from_bstr(location).into_owned()`.

Working-tree status (staged + unstaged + untracked-not-ignored):

```rust
let items = repo
    .status(gix::progress::Discard)?
    .untracked_files(gix::status::UntrackedFiles::Files)
    .into_iter(std::iter::empty::<gix::bstr::BString>())?;
// each item.location() is a repo-relative &BStr; UntrackedFiles::Files already
// excludes gitignored entries.
```

- **Cargo features**: beyond `dirwalk` + `status`, tree diffing needs `revision`
  (`rev_parse_single`) and `blob-diff` (tree change iteration). Add both.

### 2. goal-not-filter: `--since` is a selection filter, not an expansion filter

`--since` narrows the *goals*. Prerequisite expansion
(`expand_with_dependencies`, upward `Direction::Outgoing`) must stay untouched so
prereqs of affected packages still run. (Same principle as the earlier
`root-task-exclusion-and-global-expansion-skip` solution.)

### 3. The selection-matrix bypass bug (silent no-op)

`package_matches` resolves selection through a 4-arm matrix over
`(top_level, match_all_non_root_packages)`. The since filter must apply to
**every non-root arm**. Folding `passes_since` only into
`matches_non_root_package` was wrong: the `(false, true) => !is_root` arm (the
common "no `-p` given" case) returned `!is_root` directly and **bypassed the
since filter entirely**, so `--since` did nothing in the most common invocation —
and the happy-path tests still passed.

Correct shape:

```rust
let base_match = match (criteria.top_level, criteria.match_all_non_root_packages) { /* 4 arms */ };
if is_root { return base_match; }                       // root tasks bypass --since
let passes_since = criteria.since_affected
    .map_or(true, |set| set.contains(&task_id.package));
base_match && passes_since
```

### 4. The root package absorbs every repo-root file

The workspace-root `PackageNode.path == repo root`, so naive
`abs_path.strip_prefix(node.path)` matching attributes *every* changed path
(including a top-level `README.md`) to the root package, making the affected set
non-empty and breaking the no-op. Fix: exclude the root package
(`package_graph.root_package()`) from path→package candidate matching. Repo-root
files then map to no package. Always match with `strip_prefix` (never substring),
and pick the **deepest** matching package when packages nest.

### 5. `-T` + empty affected set must NOT no-op

Top-level/root tasks bypass the since filter, so a `-T` request must still run
its root task even when no package changed. The empty→no-op early return has to
be gated:

```rust
if affected.is_empty() && !selection.top_level {
    // print "No packages changed since <ref>; nothing to run." and return NoOp
}
// with -T, Proceed(Some(empty)) so package_matches selects the (bypassing) root tasks
```

Without the `!selection.top_level` guard, `luchta run <task> -T --since <ref>`
after a repo-root-only change wrongly printed "nothing to run".

## Testing notes

- Build temp git repos with the real `git` CLI (mirrors the `TestRepo` helper in
  `luchta-cache`); read them with `gix`. Build the `PackageGraph` *after* writing
  the `package.json` files, since `PackageGraph::build` re-reads them from disk.
- Prefer `--dry-run` in integration tests to assert the selected task set
  (`a#build`, `#build`, etc.) without executing scripts.
- Cover all change kinds (committed/staged/unstaged/untracked), gitignored
  exclusion, transitive dependents, the no-op, `-p ∩ --since`, invalid-ref and
  non-git errors, the regression baseline (no `--since`), and both `-T`
  interactions (non-empty AND root-only/empty affected set).
