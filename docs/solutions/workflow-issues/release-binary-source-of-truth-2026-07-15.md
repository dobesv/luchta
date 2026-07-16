---
title: "Release archive shipped missing binaries due to drifted hand-maintained lists"
date: 2026-07-15
category: workflow-issues
problem_type: workflow_issue
component: release-pipeline
root_cause: duplicated binary lists across five locations with no synchronization
resolution_type: code_fix
severity: high
tags:
  - release
  - single-source-of-truth
  - cargo-metadata
  - ci-cd
  - drift-prevention
plan_ref: release-binary-source-of-truth
---

## Problem

luchta v0.1.22 GitHub release tarball was missing `luchta-ast-grep-worker` and `luchta-worker-watcher`. The shippable binary set was duplicated across FIVE hand-maintained lists that drifted: release.yaml (build `-p` list, archive `cp` loop, smoke-test probes) + install.sh (`KNOWN_BINARY_NAMES`) + install.ps1 (`$Script:KnownBinaryNames`). Adding a crate required editing all five; missing the build `-p` list meant the binary never compiled.

## Symptoms

- Release tarball absent `luchta-ast-grep-worker`, `luchta-worker-watcher`
- User installs failed: `luchta ast-grep` commands errored with binary-not-found
- Gap discovered post-release during manual verification
- Adding new binaries required coordinated edits across 5 files

## Investigation Steps

1. Compared release tarball contents against workspace crates — identified missing crates
2. Traced release.yaml build job: used explicit `-p crate1 -p crate2` package list
3. Traced archive job: manually listed `cp` commands for each binary
4. Traced installers: hardcoded `KNOWN_BINARY_NAMES` arrays
5. Smoke tests: explicit per-binary probes (separate from build list)
6. Root cause: no canonical source of truth; each list maintained independently

## Root Cause

Binary enumeration was duplicated across 5 locations with no synchronization mechanism:
1. `release.yaml` build step: manual `-p` package arguments
2. `release.yaml` archive step: manual `cp` per binary
3. `release.yaml` smoke-test: manual probe list
4. `install.sh`: `KNOWN_BINARY_NAMES` array
5. `install.ps1`: `$Script:KnownBinaryNames` array

Lists inevitably drifted. Adding `luchta-ast-grep-worker` to archive missed the build `-p` list → binary never compiled.

## Solution

Created `cargo xtask list-release-bins` as single source of truth:

```rust
// xtask/src/main.rs
fn list_release_bins() -> Result<Vec<String>> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    
    let mut bins: Vec<String> = metadata
        .packages
        .iter()
        .flat_map(|pkg| pkg.targets.iter())
        .filter(|t| t.kind.iter().any(|k| k == "bin"))
        .filter(|t| t.name == "luchta" || t.name.starts_with("luchta-"))
        .map(|t| t.name.clone())
        .collect();
    
    bins.push("luchta-tsc-worker".to_string()); // Go-built, not in cargo metadata
    bins.sort();
    bins.dedup();
    Ok(bins)
}
```

release.yaml changes:
- Build: `cargo build --release --workspace --bins` (all bins, filter at archive)
- Archive: loop over `$(cargo xtask list-release-bins)`
- Empty-archive guard: fail if list empty
- Smoke drift guard: assert `probed_bins` sorted-set equals `list-release-bins` output

Installer changes:
- install.sh: drop `KNOWN_BINARY_NAMES`, install all files in extracted archive
- install.ps1: drop `$Script:KnownBinaryNames`, install all `*.exe` in extracted archive

## Why This Works

Single source of truth eliminates drift:
1. `list-release-bins` is the only authority for "what ships"
2. release.yaml uses it directly (via shell command)
3. Installers install everything in the archive (archive = source of truth)
4. Drift guard fails CI if probe set diverges

Adding a new binary requires ONE change: ensure it matches the filter. No coordination.

## Non-Obvious Gotchas

### Filter on bin TARGET name, NOT package name

Package `luchta-cli` ships binary `luchta`. Filtering `package.name` would drop the flagship CLI. Must filter `target.name`.

### `== "luchta"` clause is essential

`luchta` does NOT start with `luchta-`. The prefix filter alone excludes it. Must use:
```rust
.filter(|t| t.name == "luchta" || t.name.starts_with("luchta-"))
```

### Secondary bin targets require exclusion

`luchta-worker-watcher` declares a second `[[bin]]` (`mock-worker-delegate`, a test helper). The prefix filter excludes it (starts with `mock-`).

### Go-built binaries absent from cargo metadata

`luchta-tsc-worker` is Go-built, not a cargo crate. Must be appended explicitly to the bin list.

### Drift guard must use order-insensitive comparison

`list-release-bins` returns sorted output. Hand-authored probe arrays aren't sorted. An order-sensitive `!=` would fail every release. Use sorted-set compare:
```yaml
- name: Smoke test drift guard
  run: |
    expected=$(cargo xtask list-release-bins | tr '\n' ' ' | sed 's/ $//')
    probed="${{ env.probed_bins }}"
    # Sort both for order-insensitive compare
    diff <(echo "$expected" | tr ' ' '\n' | sort) <(echo "$probed" | tr ' ' '\n' | sort)
```

### install.sh must avoid GNU-only `find -printf`

End-user machines may run BSD/macOS `find`. Use portable form:
```bash
find "$extract_dir" -maxdepth 1 -type f -exec basename {} \;
```

Not:
```bash
find "$extract_dir" -maxdepth 1 -type f -printf '%f\n'  # GNU-only
```

## Prevention Strategies

### Test Coverage

- xtask unit tests cover: inclusion (`luchta`, `luchta-*`), exclusion (`mock-*`), target-name filtering, sorting dedup
- Smoke drift guard as CI runtime check
- Empty-archive guard prevents silent zero-bin release

### Code Review Checklist

- [ ] Any new binary matches `luchta` / `luchta-*` filter?
- [ ] Non-Rust binaries appended to list-release-bins?
- [ ] Release archive built from `list-release-bins` output?
- [ ] Installers install everything in archive (no hardcoded lists)?

### Monitoring

- CI drift guard fails release if probe set diverges from canonical list
- Empty-archive guard fails build if metadata error yields zero bins

## Related Issues

- **Plan:** release-binary-source-of-truth — This implementation
- **Flaky tests (unrelated):** `luchta-cache` has parallel test flakes (chdir into shared temp git state). Passes single-threaded or under nextest process isolation. Not related to release pipeline changes.
