---
title: "Cross-package input path collision made tasks permanently uncacheable"
date: 2026-06-26
category: logic-errors
problem_type: logic_error
component: luchta-cache/input-resolution
root_cause: "Input FileEntry.path stored base-relative, causing cross-package inputs with identical relative filenames to collapse to same BTreeMap key in files_diff"
resolution_type: code_fix
severity: high
tags:
  - cache-invalidation
  - cross-package-inputs
  - path-resolution
  - fingerprinting
  - BTreeMap-collapse
plan_ref: xpkg-input-path-collision
---

## Problem

Cross-package inputs sharing the same relative filename (e.g., `src/schema.graphql` in both `pkg-a` and `pkg-b`) collapsed to identical keys in the input fingerprint comparison, causing `files_diff` to report false positives on every build. Tasks with cross-package inputs were permanently uncacheable.

## Symptoms

- Tasks with cross-package inputs always re-ran (`Decision::Run`) regardless of whether inputs changed
- Cache key was stable (correct) but fingerprint comparison always reported "changed"
- Behavior was nondeterministic: parallel merge order in `resolve_inputs_with_semantics` determined which duplicate survived
- Fully content-independent: even identical content caused re-runs due to map collapse

```
# Example: two packages each have src/schema.graphql
# Input resolution returns two entries:
pkg-a/src/schema.graphql  # hash A
pkg-b/src/schema.graphql  # hash B

# But files_diff builds BTreeMap<&str path, &FileEntry>
# Keys collide → one entry survives
# Different survivors in prior vs current → "changed"
```

## Investigation Steps

1. Enabled debug logging, observed `Decision::Run` on every invocation for cross-package tasks
2. Traced to `decide.rs::files_diff` — builds `BTreeMap<&str path, &FileEntry>` keyed on `FileEntry.path`
3. Found `FileEntry.path` was base-dir-relative, not repo-relative
4. Each cross-package input (`pkg#...`, `^...`) has its own `base_dir`, so two packages containing `src/schema.graphql` both produced key `"src/schema.graphql"`
5. Map insertion order from parallel merge was nondeterministic → prior vs current kept different survivors
6. Root cause: path field must be globally unique across all sources merged into comparison

## Root Cause

`FileEntry.path` was stored as base-dir-relative. The `files_diff` function built a `BTreeMap<&str, &FileEntry>` keyed on `.path`. When two entries shared the same relative path from different packages, BTreeMap collapsed them. The survivor depended on parallel merge order, making the fingerprint comparison nondeterministic and always-false on repeated runs.

## Solution

**Stage 1: Qualify input paths repo-relative**

Changed input `FileEntry.path` to be repo/worktree-relative (e.g., `pkg-a/src/schema.graphql`). The `worktree_relative_base_dir` helper already computed the repo-relative prefix for each `base_dir` — reused it to qualify paths at resolution time.

```rust
// resolve.rs - file_entry_from_path
fn file_entry_from_path(
    base: ResolvedBase<'_>,
    relative_path: PathBuf,
    file_reader: &dyn FileReader,
) -> Result<FileEntry> {
    let absolute_path = base.dir.join(&relative_path);
    let qualified_path = qualify_relative_path(base.prefix, &relative_path);
    // ... rest of function
}

fn qualify_relative_path(base_dir_prefix: &Path, relative_path: &Path) -> String {
    let qualified = if base_dir_prefix.as_os_str().is_empty() {
        relative_path.to_path_buf()
    } else {
        base_dir_prefix.join(relative_path)
    };
    normalize_path(&qualified)
}
```

Outputs deliberately kept package-relative. They can't collide cross-package within one task, and package-relative preserves snapshot/restore semantics.

**Stage 2: Fix dedup path reconstruction**

`dedupe_and_sort_entries` needed to reconstruct absolute paths for canonical-identity dedup. After Stage 1, entry paths were repo-relative. First implementation used fragile ancestor-walk heuristic:

```rust
// WRONG: heuristic could canonicalize wrong physical file
// in nested layouts like /repo/pkg-a/pkg-a/src/file.txt
let absolute = ancestors(entry_path)
    .find(|p| p.exists())
    .and_then(|p| p.join(entry_path));
```

Robust fix: derive worktree root deterministically by stripping the repo-relative prefix from `base_dir`:

```rust
// resolve.rs - resolve_inputs_with_semantics
let base_dir_prefix = qualified_base_dir_prefix(&request.base_dir, &mut cache)?;
// Strip known prefix to get exact worktree root
let worktree_root = strip_suffix_components(&request.base_dir, &base_dir_prefix);

fn strip_suffix_components(path: &Path, suffix: &Path) -> PathBuf {
    let suffix_len = suffix.components().count();
    let mut result = path.to_path_buf();
    for _ in 0..suffix_len {
        if !result.pop() { break; }
    }
    result
}
```

Then canonical dedup uses exact join — no probing:

```rust
fn canonical_dedupe_key(worktree_roots: &[PathBuf], entry_path: &str) -> Option<String> {
    let entry_path = Path::new(entry_path);
    worktree_roots
        .iter()
        .map(|root| root.join(entry_path))
        .find_map(|path| fs::canonicalize(path).ok())
        .map(|path| normalize_path(&path))
}
```

This reverses the prefixing exactly: `(base_dir - prefix) + (prefix + relative) = base_dir + relative`.

## Why This Works

1. **Repo-relative paths are globally unique**: Two packages with `src/schema.graphql` become `pkg-a/src/schema.graphql` and `pkg-b/src/schema.graphql`. BTreeMap keys don't collide.

2. **Deterministic key assignment**: Each path resolves to one unique key regardless of parallel merge order.

3. **Exact path reconstruction**: Stripping known suffix avoids heuristic probing. The worktree root is computed from known quantities, making dedup resilient to nested-directory shadowing.

## Prevention Strategies

**Test Cases Added:**

- `resolve_inputs_with_semantics_distinguishes_same_relative_path_across_packages`: Two packages each have `src/schema.graphql`, verify two distinct qualified paths
- `files_diff_same_qualified_cross_package_paths_report_no_change`: Same inputs in different order → `files_changed` returns false
- `decide_skip_with_qualified_cross_package_inputs`: Prior record with qualified paths → `decide()` returns `Decision::Skip`
- `resolve_inputs_with_semantics_dedup_unaffected_by_shadow_directory`: Nested directory mirrors package name (`pkg-a/pkg-a/...`), verify dedup ignores shadow path

**Best Practices:**

- When a cache/fingerprint comparison keys a map on a path field, that field MUST be globally unique across all sources merged into the comparison
- Changing stored path relativity requires auditing every consumer that reconstructs absolute paths
- Prefer deterministic reconstruction (strip-known-prefix → exact root join) over heuristic probing (ancestor walk / first-exists)

**Code Review Checklist:**

- [ ] Are path keys globally unique across all merge sources?
- [ ] Does path reconstruction use deterministic logic (no filesystem probing)?
- [ ] Are there tests for shadow-directory edge cases?

**Monitoring:**

- Sudden spike in cache miss rate for cross-package tasks → investigate path qualification

## Related Issues

- **GitHub:** Issue #138 — Cross-package input path collision
- **Related Solution:** [security-issues/cross-package-input-expansion-security-2026-06-16.md](../security-issues/cross-package-input-expansion-security-2026-06-16.md) — Path-escape hard-fail for untrusted worker patterns
- **Related Solution:** [logic-errors/detected-patterns-flag-conflation-2026-06-12.md](../logic-errors/detected-patterns-flag-conflation-2026-06-12.md) — Pattern selection parity between decide/write paths

## Notes

Changing recorded path strings is a safe one-time cache miss, not corruption. String mismatch → "changed" → recompute. No schema bump strictly required, though conceptually this is a format change.
