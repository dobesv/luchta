---
title: "Standard Rust xtask automation pattern with metadata-driven install"
date: 2026-06-10
category: "workflow-issues"
problem_type: workflow_issue
component: "xtask"
root_cause: "n/a — new capability addition"
resolution_type: workflow_improvement
severity: low
tags:
  - rust
  - cargo-workspace
  - automation
  - xtask
  - cargo-metadata
plan_ref: "xtask-helper"
---

## Problem

The project lacked a standard mechanism for project-specific automation tasks. Developers needed a one-liner to install all workspace binary crates, and the solution needed to stay correct automatically as crates are added without maintaining hardcoded lists.

## Solution

Implemented the standard Rust `xtask` pattern:

### 1. Cargo Alias

`.cargo/config.toml`:
```toml
[alias]
xtask = "run --package xtask --"
```

This enables `cargo xtask <subcommand>` invocation from anywhere in the workspace.

### 2. xtask Binary Crate

Added `xtask` as a workspace member (binary crate):

```toml
[package]
name = "xtask"
version.workspace = true
publish = false              # internal dev tool, not for crates.io

[[bin]]
name = "xtask"
path = "src/main.rs"

[dependencies]
clap = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
```

### 3. Install Subcommand Implementation

The `install` subcommand discovers workspace binary crates dynamically via `cargo metadata`:

```rust
fn workspace_bin_packages(metadata: &Metadata) -> Vec<WorkspaceBinPackage> {
    let mut packages: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .filter(|package| package.name != env!("CARGO_PKG_NAME"))  // self-exclusion
        .filter(|package| package.targets.iter().any(Target::is_bin))
        .filter_map(|package| {
            crate_dir(&package.manifest_path).map(|crate_dir| WorkspaceBinPackage {
                name: package.name.clone(),
                crate_dir,
            })
        })
        .collect();

    packages.sort_by(|left, right| left.name.cmp(&right.name));
    packages
}
```

Then runs `cargo install --path <dir>` for each discovered crate.

### Why This Works

- **Dynamic discovery**: `cargo metadata --format-version 1 --no-deps` returns all workspace packages; filtering by `workspace_members` and `kind: ["bin"]` picks installable targets. New crates join automatically.
- **Self-exclusion**: `env!("CARGO_PKG_NAME")` excludes whichever crate this binary is compiled as — no brittle `"xtask"` string literal.
- **Toolchain consistency**: Invoke cargo via `std::env::var("CARGO")` (set by cargo when run through the alias), falling back to `"cargo"` — stays on the same toolchain.
- **Minimal deps**: Hand-rolled `{ packages, workspace_members }` serde structs suffice; avoids pulling the heavier `cargo_metadata` crate.

### Robustness Details from Review

- **Self-exclusion**: Uses `env!("CARGO_PKG_NAME")` instead of hardcoded `"xtask"`.
- **Cargo invocation**: Uses `$CARGO` env var when available (set by cargo during alias execution), falling back to `"cargo"`.
- **Accurate summary**: Capture `total = packages.len()` up front before iterating, so success/failure messages report `installed/total` correctly (initial version had `installed/installed` bug).
- **Internal tool posture**: `publish = false` prevents accidental crates.io publication.
- **Workspace version alignment**: `version.workspace = true` follows single-version workspace convention.

### Testing Approach

Pure filtering logic (`workspace_bin_packages`) unit-tested against inline JSON fixture:

```rust
const SAMPLE_METADATA: &str = r#"{
    "packages": [
        {"id": "luchta-cli 0.1.0 (path+file:///repo/crates/luchta-cli)", "name": "luchta-cli", 
         "manifest_path": "/repo/crates/luchta-cli/Cargo.toml", "targets": [{"kind": ["bin"]}]},
        {"id": "xtask 0.1.0 (path+file:///repo/xtask)", "name": "xtask",
         "manifest_path": "/repo/xtask/Cargo.toml", "targets": [{"kind": ["bin"]}]},
        {"id": "some-dep 1.0.0 (registry+https://example.com)", "name": "some-dep",
         "manifest_path": "/cache/some-dep/Cargo.toml", "targets": [{"kind": ["bin"]}]},
        // ... more packages intentionally unsorted
    ],
    "workspace_members": ["luchta-cli 0.1.0 (path+...)", "xtask 0.1.0 (path+...)", ...]
}"#;
```

Tests cover:
- Bin vs lib target identification
- Non-member (registry dependency) exclusion
- Self-exclusion (xtask itself)
- Sort order (fixture intentionally unsorted)
- Empty/all-filtered edge cases

IO/process-spawning wrappers (`cargo_install`, `workspace_metadata`) left thin and untested — appropriate for internal dev tool.

### Decisions Deliberately Not Taken

Rejected in review:
- **No `thiserror`**: Binary dev tool with `Result<T, String>` is adequate — no library API affected.
- **No `--force` on install**: Changes semantics; default cargo behavior (skip if version matches) is correct for idempotent CI/automation.
- **No `--locked` on install**: `cargo install --path` in workspace already uses local `Cargo.lock`; unnecessary.

## Prevention Strategies

- **Test coverage**: Unit test pure logic (`workspace_bin_packages`) against representative metadata fixtures.
- **Fixture ordering**: Make fixtures intentionally unsorted so sort step is exercised.
- **Edge case tests**: Empty workspace, all-filtered (lib-only + xtask only), non-member bins from registry dependencies.
- **Self-exclusion via compile-time constant**: `env!("CARGO_PKG_NAME")` not string literal.

## Related Issues

- GitHub Issue: #29 — Setup xtask helper for install and other little tasks
- Changeset: `.changeset/add-xtask-automation-crate.md` (front-matter key `luchta: minor`)
