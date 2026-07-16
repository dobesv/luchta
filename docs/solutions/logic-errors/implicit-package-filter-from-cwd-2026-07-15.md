---
title: "Implicit package filter from CWD: workspace root walk, boundary detection, and CLI wiring"
date: 2026-07-15
category: logic-errors
problem_type: logic_error
component: luchta-cli, luchta-workspace
root cause: "resolve_workspace_root used cwd directly without upward walk; relative vs absolute path mismatch in boundary detection"
resolution_type: code_fix
severity: medium
tags:
  - cli
  - task-selection
  - package-filtering
  - workspace-discovery
  - goal-not-filter
  - path-resolution
plan_ref: luchta-implicit-package-filter
---

## Problem

Running `luchta run/watch/logs/why` from within a package subdirectory should automatically scope the command to that package (as if `-p <name>` were passed). However, two logic errors prevented correct behavior:

1. `resolve_workspace_root` used `cwd` directly without walking upward for a `package.json` with a `workspaces` field, causing subdir misclassification as workspace root.
2. `detect_implicit_package` compared an absolute `cwd` against a potentially relative `workspace_root`, silently disabling implicit detection when `--workspace-root` was passed as a relative path.

## Symptoms

- From a package subdirectory, `luchta run <task>` would incorrectly treat the subdir as workspace root, failing workspace discovery.
- Passing `--workspace-root .` or `--workspace-root ..` would silently disable implicit package detection (no error, just unscoped run).
- Workers remained pinned correctly, but task selection was wrong.

## Investigation Steps

1. Identified that `resolve_workspace_root` (run.rs) returned `cwd` directly when no `--workspace-root` flag — no upward walk.
2. Found precedent: `crates/luchta-oxlint-worker/src/config.rs:128` uses `Path::ancestors()` walk for workspace root detection.
3. Traced `detect_implicit_package` boundary check: `cwd.starts_with(workspace_root)` fails when `cwd` is absolute (from `current_dir()`) and `workspace_root` is relative (from CLI flag).
4. Review notes (Calliope, Urania) flagged relativity bug; judges (Minos, Rhadamanthus, Aeacus) agreed real but edge-case and safe-failing.

## Root Cause

### Workspace Root Resolution

`resolve_workspace_root` assumed the caller was already at workspace root. When invoked from `packages/ui/src/components/`, it returned that subdir, not the monorepo root.

**Before:**
```rust
pub fn resolve_workspace_root(workspace_root: Option<PathBuf>) -> Result<PathBuf> {
    match workspace_root {
        Some(path) => Ok(path),
        None => std::env::current_dir().into_diagnostic(),
    }
}
```

### Path Relativity in Boundary Check

`detect_implicit_package` compared paths lexically without normalizing. An absolute `cwd` would never `starts_with` a relative `workspace_root`.

**Before:**
```rust
pub fn detect_implicit_package(cwd: &Path, workspace_root: &Path) -> Option<String> {
    if cwd == workspace_root || !cwd.starts_with(workspace_root) {
        return None;
    }
    // ... ancestor walk
}
```

## Solution

### 1. Upward walk for workspace root

Changed `resolve_workspace_root` to walk `cwd.ancestors()` looking for first `package.json` with `workspaces` field:

```rust
pub fn resolve_workspace_root(workspace_root: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(workspace_root) = workspace_root {
        return Ok(workspace_root);
    }

    let cwd = std::env::current_dir().into_diagnostic()?;
    for ancestor in cwd.ancestors() {
        if has_workspaces_field(ancestor) {
            return Ok(ancestor.to_path_buf());
        }
    }

    Ok(cwd)
}

fn has_workspaces_field(path: &Path) -> bool {
    let package_json = match std::fs::read_to_string(path.join("package.json")) {
        Ok(content) => content,
        Err(_) => return false,
    };

    match serde_json::from_str::<WorkspaceRootPackageJson>(&package_json) {
        Ok(pkg) => pkg.workspaces.is_some(),
        Err(_) => false,
    }
}
```

### 2. Canonicalize both paths before boundary check

Changed `detect_implicit_package` to canonicalize both paths. If either fails, return `None` gracefully:

```rust
pub fn detect_implicit_package(cwd: &Path, workspace_root: &Path) -> Option<String> {
    let cwd = cwd.canonicalize().ok()?;
    let workspace_root = workspace_root.canonicalize().ok()?;

    if cwd == workspace_root || !cwd.starts_with(&workspace_root) {
        return None;
    }

    for ancestor in cwd.ancestors() {
        if ancestor == workspace_root {
            break;
        }

        let package_name = find_package_name_at(&ancestor.join("package.json"));
        if package_name.is_some() {
            return package_name;
        }
    }

    None
}
```

### 3. Single helper for CLI wiring

Added `apply_implicit_package` in `main.rs` to centralize detection across all four commands:

```rust
fn apply_implicit_package(
    packages: Vec<String>,
    top_level: bool,
    workspace_root: &Path,
) -> Result<Vec<String>> {
    if top_level || !packages.is_empty() {
        return Ok(packages);
    }

    let cwd = std::env::current_dir().into_diagnostic()?;
    Ok(run::detect_implicit_package(&cwd, workspace_root)
        .map(|package| vec![package])
        .unwrap_or(packages))
}
```

Called from `Run`, `Watch`, `Logs`, `Why` arms with identical guard logic.

### 4. Clean API for package name lookup

Added `find_package_name_at` to `luchta-workspace/src/discovery.rs`:

```rust
pub fn find_package_name_at(path: &Path) -> Option<String> {
    read_package_json(path)
        .ok()
        .and_then(|pkg| pkg.name)
        .filter(|name| !name.is_empty())
}
```

Re-exported from `lib.rs` for CLI access.

## Why This Works

### Upward Walk

Matches the existing pattern in `luchta-oxlint-worker/src/config.rs:128`. The walk stops at the first `package.json` with `workspaces` field, which is the workspace root marker. If none found, falls back to cwd (original behavior).

### Canonicalization

`std::path::canonicalize` resolves relative paths, symlinks, and `.`, `..` components. Both paths become absolute, making `starts_with` comparison correct. Graceful `None` on failure maintains fail-safe behavior.

### Root Exclusion

The ancestor walk stops *before* checking `workspace_root` itself. Running from root returns `None` immediately via the `cwd == workspace_root` check.

### Raw-Name Passthrough

The detected package name is NOT validated against discovered packages. Unknown names flow to existing `package_matches` error path — same as explicit `-p <unknown>`. This aligns with goal-not-filter model (see related solutions).

### Escape Hatch

`-p '*'` or explicit `-p` bypasses implicit detection. No new flag needed.

## Prevention Strategies

**Path Resolution Patterns:**

- [ ] When walking upward for configuration, use `Path::ancestors()`
- [ ] Always canonicalize/absolutize before lexical path comparison
- [ ] Graceful fallback on canonicalization failure (return default, not error)

**Boundary Conditions:**

- [ ] Walk must stop before or at the boundary; exclusive upper bound
- [ ] Root package excluded by equality check before walk
- [ ] Detect when CWD is outside workspace (starts_with guard)

**CLI Wiring:**

- [ ] Single helper for cross-command behavior (DRY)
- [ ] Guard conditions: explicit packages flag AND top-level flag
- [ ] Workers pinned to workspace root unchanged (selection only)

**Testing:**

- [ ] Unit tests with `tempfile::TempDir` and explicit path arguments (no global CWD mutation)
- [ ] Canonicalize test paths before comparison (avoid macOS `/var` vs `/private/var` alias)
- [ ] Use `cargo nextest` for process-isolated CWD-dependent tests

## Related Issues

- **Related Solution:** [cli-package-targeting-codescene-whack-a-mole-2026-06-15.md](cli-package-targeting-codescene-whack-a-mole-2026-06-15.md) — Goal-not-filter model, selection matrix, `package_matches` invariants
- **Related Solution:** [since-filter-selection-and-gix-change-detection-2026-06-18.md](since-filter-selection-and-gix-change-detection-2026-06-18.md) — Selection-matrix bypass bug, `--since` invariants
- **GitHub:** [#209](https://github.com/dobesv/luchta/issues/209) — Implicit package filter if run in a package folder
