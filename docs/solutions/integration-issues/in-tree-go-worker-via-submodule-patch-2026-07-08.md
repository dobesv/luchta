---
title: "In-tree Go worker build via git submodule + binary patch"
date: 2026-07-08
last_updated: 2026-07-08
category: integration-issues
problem_type: integration_issue
component: xtask, vendor/tsgo, release workflow
root_cause: "n/a — new integration pattern (vendor upstream fork via submodule+patch, build Go worker without copying source)"
resolution_type: workflow_improvement
severity: medium
tags:
  - go
  - git-submodule
  - git-patch
  - binary-patch
  - crlf
  - core.autocrlf
  - github-actions
  - set-e
  - cross-compilation
  - release-archives
plan_ref: luchta-193-tsc-worker-in-tree
---

## Problem

The Go `luchta-tsc-worker` binary was only available in a fork (`dobesv/typescript-go@9ed9a7d`), not in the main repo. Needed to build and ship the worker inside luchta's 7 GitHub release archives WITHOUT copying the entire typescript-go codebase into the repo. Required: small repo footprint, reproducible builds, CI integration, and drift detection when the patch rots.

## Symptoms

- Pre-integration: Worker only available by cloning fork separately; no shipping mechanism
- During integration: `git apply --check` passed in working copy, then failed after fresh checkout with "patch does not apply"
- CI workflow: Patch-drift detection logic never reached issue-creation branch when patch failed — step aborted silently

## Investigation Steps

1. Identified merge-base (`e578159b`) between upstream `microsoft/typescript-go` main and fork commit `9ed9a7d` using temporary scratch repo with `git merge-base upstream/main 9ed9a7d`
2. Added submodule pointing to upstream (not fork), pinned to merge-base SHA
3. Generated patch via `git diff --binary e578159b..9ed9a7d > patches/tsgo.patch`
4. Built worker via `cargo xtask build-worker --target <triple>` with hardcoded Rust-triple→Go-env table
5. Integrated into release.yaml: build on runner HOST (not inside cross Docker container), bundle into existing archive loop
6. Added `.gitattributes` entry after CRLF bug discovered; regenerated patch

## Root Cause Analysis (for gotchas)

### Gotcha 1: CRLF + core.autocrlf=input

The fork source (and thus the patch context/added lines) contains CRLF. Repo has `core.autocrlf=input`. On `git add`, autocrlf STRIPPED the CR from the stored patch blob. On checkout, the patch lost CRLF, and `git apply` failed because context lines no longer matched the submodule's CRLF source.

**Working copy hid the bug**: `git apply --check` passed because the working copy still had CRLF. Only fresh checkout (`rm patches/tsgo.patch && git checkout -- patches/tsgo.patch`) revealed the failure.

### Gotcha 2: GitHub Actions bash -eo pipefail

GitHub Actions default bash shell is `bash --noprofile --norc -eo pipefail` (`set -e` ON). A `OUTPUT=$(cmd_that_may_fail)` + `$?` pattern ABORTS the step immediately when `cmd` fails — the subsequent `$?` line never executes. Intended "detect failure then act" workflow (patch-drift: check patch, if fail then create issue) never reached issue-creation branch.

### Gotcha 3: Submodule must pin to merge-base, not fork HEAD

Vendoring via submodule+patch requires pinning to the MERGE-BASE of upstream vs fork. Pinning to fork HEAD causes the patch to not apply (patch generated against merge-base). Pinning to upstream latest causes similar issues.

## Solution

### Directory Structure

```
vendor/tsgo          → git submodule (pointer only, no source committed)
patches/tsgo.patch   → binary diff from merge-base to fork HEAD (~7500 lines)
.gitattributes       → patches/tsgo.patch -text (preserve CRLF byte-for-byte)
```

### 1. .gitattributes (CRITICAL)

```gitattributes
patches/tsgo.patch -text
```

This ensures the patch is stored and checked out byte-for-byte, preserving CRLF. Without this, `core.autocrlf=input` corrupts the patch on add/checkout.

**Verify with fresh checkout:**
```bash
rm patches/tsgo.patch
git checkout -- patches/tsgo.patch
git -C vendor/tsgo apply --check ../../patches/tsgo.patch
```

### 2. Submodule Addition

```bash
git submodule add https://github.com/microsoft/typescript-go vendor/tsgo
cd vendor/tsgo && git checkout e578159b7ae473127056a65748d7b3a4daa9a93f
# Add shallow = true in .gitmodules for efficiency
```

Pinned to merge-base SHA, not fork HEAD.

### 3. Patch Generation

```bash
# In temp clone with both remotes
git diff --binary e578159b..9ed9a7d > patches/tsgo.patch
```

Always use `--binary` to preserve CRLF in the patch.

### 4. xtask build-worker Pattern

`cargo xtask build-worker --target <rust-triple> [--out-dir <dir>]`:

- Hardcoded Rust-triple→Go-env table (no parser)
- Build flags: `CGO_ENABLED=0 -trimpath -ldflags "-s -w"`
- Idempotent reset→apply→build→reset-clean of submodule
- Output: `luchta-tsc-worker(.exe)` to `target/<triple>/release`
- No build.rs; Rust build/tests never require Go

```rust
// Key pattern: always reset submodule to clean state before patching
fn build_worker(target: &str, out_dir: &Path) -> Result<()> {
    let submodule = repo_root.join("vendor/tsgo");
    
    // Idempotent reset
    run_cmd("git", ["-C", &submodule, "checkout", "."])?;
    run_cmd("git", ["-C", &submodule, "clean", "-fd"])?;
    
    // Gate with apply --check
    run_cmd("git", ["-C", &submodule, "apply", "--check", "../../patches/tsgo.patch"])?;
    run_cmd("git", ["-C", &submodule, "apply", "../../patches/tsgo.patch"])?;
    
    // Build
    let go_env = go_env_for_target(target);
    run_cmd_env("go", ["build", "-o", &output_name, "./cmd/tsc-worker"], go_env)?;
    
    // Reset clean
    run_cmd("git", ["-C", &submodule, "checkout", "."])?;
    run_cmd("git", ["-C", &submodule, "clean", "-fd"])?;
    
    Ok(())
}
```

### 5. release.yaml Integration

```yaml
# Build Go worker on HOST (never inside cross Docker container)
- name: Build tsc-worker (${{ matrix.target }})
  if: steps.skip.outputs.skip != 'true'
  run: cargo xtask build-worker --target ${{ matrix.target }}
  env:
    GOOS: ${{ matrix.goos }}
    GOARCH: ${{ matrix.goarch }}
```

Worker builds into `target/<target>/release` where existing archive cp loop already picks it up.

**Smoke-exec only on native targets:**
```yaml
- name: Smoke test tsc-worker
  if: steps.skip.outputs.skip != 'true' && matrix.cross == ''
  run: target/${{ matrix.target }}/release/luchta-tsc-worker --version
```

Never exec cross-compiled binary (won't run on host).

### 6. patch-drift.yaml (Weekly Check)

```yaml
- name: Check patch applies
  id: check
  shell: bash
  run: |
    # CRITICAL: Use if ! to trap failure under set -e
    if ! OUTPUT=$(git -C vendor/tsgo apply --check ../../patches/tsgo.patch 2>&1); then
      echo "patch_applies=false" >> $GITHUB_OUTPUT
      EXIT_STATUS=$?
    else
      echo "patch_applies=true" >> $GITHUB_OUTPUT
      EXIT_STATUS=0
    fi
    
    # Multiline output for issue body
    {
      echo "patch_check_output<<EOF"
      echo "$OUTPUT"
      echo "EOF"
    } >> $GITHUB_OUTPUT
    
    exit $EXIT_STATUS
```

**Key pattern:** `if ! OUTPUT=$(...); then ...` traps failure WITHOUT aborting the step, allowing issue-creation logic to run. Plain `OUTPUT=$(...)` + `$?` ABORTS under `set -e`.

### 7. Runtime Discovery

Worker ships beside `luchta` in archive. Discovery via PATH:

```bash
# User extracts archive, adds to PATH
export PATH="/path/to/extracted:$PATH"
luchta run  # spawns luchta-tsc-worker via sh -c, resolves via PATH
```

No engine/luchta-config changes required.

## Why This Works

1. **Submodule pointer-only**: Only commits the SHA, not source files. Keeps repo small (pointer + ~7500-line patch vs 50k+ line codebase).

2. **Merge-base pinning**: Submodule at merge-base + patch from merge-base to fork HEAD = patch applies cleanly. Rebasing onto newer upstream is future work, not accidental.

3. **Binary patch + .gitattributes**: `patches/tsgo.patch -text` preserves CRLF byte-for-byte through add/checkout so patch context matches submodule's CRLF source.

4. **Reset-apply-build-reset cycle**: Submodule stays clean in repo; patch applied only during build, reset afterwards. No committed changes to submodule.

5. **Host-side Go build**: Cross-compilation via GOOS/GOARCH on runner host avoids cross container limitations (Go may not be installed, architecture mismatch for exec).

6. **set -e trap pattern**: `if ! OUTPUT=$(may-fail)` lets failure handling code run under `-eo pipefail`.

## Prevention Strategies

- **Test Cases:**
  - Verify `git apply --check` from FRESH checkout: `rm patches/tsgo.patch && git checkout -- patches/tsgo.patch && git -C vendor/tsgo apply --check ../../patches/tsgo.patch`
  - Weekly patch-drift workflow must FAIL (open issue) when patch doesn't apply
  - Integration test: build worker for each target, verify output exists

- **Best Practices:**
  - Always use `.gitattributes` `-text` for patches that may contain CRLF
  - Always generate patch with `git diff --binary`
  - Always verify patch from fresh checkout, not just working copy
  - Use `if ! OUTPUT=$(...)` pattern in GitHub Actions to trap failure under `set -e`
  - Pin submodule to merge-base (not fork HEAD, not upstream latest)

- **Code Review Checklist:**
  - [ ] Does patch contain CRLF? If yes, `.gitattributes` entry present?
  - [ ] Patch verified from fresh checkout?
  - [ ] GitHub Actions failure-handling tested under `-eo pipefail`?
  - [ ] Submodule pinned to merge-base?

## Appendix A: Rebasing Onto Newer Upstream (TypeScript 7.0 GA)

The original integration pinned the submodule to merge-base `e578159b`. Rebasing onto TypeScript 7.0 GA (`typescript/v7.0.2` → `2bd066d87f5bafd315be9f40889d0a60b9e58e0b`) required deriving a new patch. This section documents the rebase procedure and key gotchas.

### Procedure

```bash
# 1. Fetch the target tag into the shallow submodule
cd vendor/tsgo
git fetch --depth=1 origin tag typescript/v7.0.2

# 2. Check out the new base
git checkout 2bd066d87f5bafd315be9f40889d0a60b9e58e0b

# 3. Attempt patch application with --reject to surface conflicts
git apply --reject --whitespace=nowarn ../../patches/tsgo.patch

# 4. Inspect .rej files — only 4 of 93 files rejected
#    Resolve each reject by hand, preserving upstream semantics

# 5. Delete .rej files after resolution
find . -name '*.rej' -delete

# 6. Stage and regenerate patch from new base
git add -A
cd ../..
git -C vendor/tsgo diff --binary --staged 2bd066d87f5bafd315be9f40889d0a60b9e58e0b > patches/tsgo.patch

# 7. Update submodule pointer
git -C vendor/tsgo checkout 2bd066d87f5bafd315be9f40889d0a60b9e58e0b

# 8. Verify from fresh checkout
rm patches/tsgo.patch && git checkout -- patches/tsgo.patch
git -C vendor/tsgo apply --check ../../patches/tsgo.patch
cargo xtask build-worker
```

### Gotcha 3: `git apply --3way` is Atomic

`git apply --3way` aborts the ENTIRE application on any single conflicting hunk. One conflict in `diagnostics_generated.go` caused a rollback of all 93 files, leaving `git status` clean — appearing as if nothing applied.

**Diagnosis:** Use `git apply --reject` instead. This materializes clean hunks and creates `.rej` files only for conflicting hunks. This revealed only 4 rejected files out of 93, making the actual conflict surface visible and bounded.

### Gotcha 4: Self-Contained Fixtures

The original patch referenced Yarn PnP runtime fixtures (`testdata/fixtures/pnp/*.cjs`) in tests but excluded their bodies:

```bash
# Old regenerate (excluded fixtures)
git diff --binary <old-base>..<fork-head> -- . '!testdata/fixtures/pnp/*.cjs'
```

On fresh checkout, tests failed with missing fixture files. The rebase bundled these fixtures (~18k lines), making vendored PnP tests self-contained.

**Patch grew from +5795 lines to +24030 lines.** This is expected — the patch must now carry fixtures to be portable.

### Gotcha 5: Semantic Reject Resolution

Reject resolution is not purely mechanical. Example: `internal/module/resolver.go`.

Upstream changed:
```go
// Before (old base)
if resolved == nil {
    return nil
}
```

To:
```go
// After (v7.0 GA)
if !resolved.Exists() {
    return nil
}
```

Our patch added optional-peer-filter logic + PnP `ResolveToUnqualified`. Resolution required merging our changes onto the new upstream form, preserving both upstream semantics (nil → `.Exists()`) and our PnP extensions.

**Always compile after resolving rejects:** `go build ./cmd/tsc-worker` must succeed.

### Gotcha 6: Dynamic Patch-Drift SHA

`patch-drift.yaml` originally hardcoded the pinned SHA. Rebases would break this.

**Fix:** Derive dynamically from submodule pointer:

```yaml
- name: Get pinned SHA
  run: echo "PINNED_SHA=$(git ls-tree HEAD vendor/tsgo | awk '{print $3}')" >> $GITHUB_OUTPUT
```

This survives rebases without manual updates.

### Upstream Status Snapshot (2026-07-08 GA)

- **Repo:** Still `microsoft/typescript-go` (no stable integration API until 7.1)
- **Yarn PnP:** NOT upstream — issue [#460](https://github.com/microsoft/typescript-go/issues/460) open, PR [#1966](https://github.com/microsoft/typescript-go/pull/1966) open-unmerged
- **Implication:** Subprocess-worker + `internal/luchta` glue remains necessary; patch concern (A) must still be carried
- **New pinned base:** `2bd066d87f5bafd315be9f40889d0a60b9e58e0b` (tag `typescript/v7.0.2`)

## Related Issues

- **GitHub Issue:** [#193](https://github.com/dobesv/luchta-tsc-worker/issues/193) — In-tree tsc-worker
- **GitHub Issue:** [#199](https://github.com/dobesv/luchta-tsc-worker/issues/199) — Rebase onto TypeScript 7.0 GA
- **Plan:** `luchta-193-tsc-worker-in-tree`
- **Plan:** `luchta-tsc-worker-v7-rebase-investigation`
- **Related Solution:** [workflow-issues/xtask-automation-pattern-2026-06-10.md](../workflow-issues/xtask-automation-pattern-2026-06-10.md) — xtask pattern for project automation
