---
title: "luchta-tsc-worker SARIF file paths aligned to repo-relative convention"
date: 2026-07-22
category: logic-errors
problem_type: logic_error
component: luchta-tsc-worker
root_cause: "tsc worker emitted absolute paths in SARIF artifactLocation.uri while all other workers already emitted repo-relative paths"
resolution_type: code_fix
severity: medium
tags:
  - sarif
  - go
  - tsc-worker
  - patch-embedded-code
  - path-normalization
  - cross-worker-consistency
plan_ref: issue-259-sarif-repo-relative
---

## Problem

The Go-based `luchta-tsc-worker` was the only worker emitting absolute file paths in SARIF `artifactLocation.uri` fields. All Rust workers (oxlint, ast-grep, oxfmt) had already been updated to emit repo-relative paths. This inconsistency broke portability of SARIF reports and violated the repo-wide convention documented in `docs/superpowers/specs/2026-07-15-repo-root-relative-diagnostic-paths-design.md`.

## Symptoms

- SARIF reports from tsc worker contained absolute paths like `/home/user/repo/packages/app/src/index.ts`
- Rust worker SARIF reports contained portable paths like `packages/app/src/index.ts`
- Downstream consumers (VS Code SARIF viewer, GitHub code scanning) could not reliably resolve paths across different checkout locations
- `luchta logs` pretty-printing had inconsistent path display for tsc diagnostics

## Investigation Steps

1. Traced SARIF output generation across all workers — confirmed tsc was the lone absolute-path emitter
2. Located the responsible code: `DiagnosticsToSARIF` in `internal/luchta/sarif.go` used `file.FileName()` directly
3. Identified the Rust convention: `luchta_worker::paths::repo_relative(path, root)` uses `strip_prefix(root).unwrap_or(path)` with forward-slash normalization
4. Discovered the Go worker source lives inside `patches/tsgo.patch` (unified diff applied to `vendor/tsgo` submodule at build time)
5. Traced `luchta-engine` worker spawn logic: engine pins each worker's CWD to `workspace_root` (repo root) via `command.current_dir(workspace_root)`

## Root Cause

Three factors combined:

1. **Source embedding**: The tsc worker's Go source is embedded as a unified diff inside `patches/tsgo.patch`. To change behavior, you edit the `+`-prefixed lines within the patch and must update hunk headers (`@@ -a,b +c,d @@`) to match the new line counts, or `git apply` fails.

2. **Convention discovery**: The repo-root = process-launch-CWD convention was implicit. Rust workers read it via `std::env::current_dir()` at startup; the Go worker needed to do the same via `os.Getwd()`. The per-task `run.Cwd` in Run messages is the _package_ directory, not the repo root — using it would produce package-relative paths (the bug).

3. **Missing helper parity**: Rust has `luchta_worker::paths::repo_relative(path, root)` in `crates/luchta-worker/src/paths.rs`. Go needed an equivalent helper that strips the repo root prefix and normalizes to forward slashes, with a fallback to full path for files outside the repo.

## Solution

Applied three coordinated changes in `patches/tsgo.patch`:

### 1. Capture repo root at worker startup (`internal/luchta/worker.go`)

```go
import (
    "os"
    "github.com/microsoft/typescript-go/internal/tspath"
)

func Serve(ctx context.Context, in io.Reader, out io.Writer, errw io.Writer) error {
    repoRoot, err := os.Getwd()
    if err != nil {
        return fmt.Errorf("luchta-tsc-worker: get working directory: %w", err)
    }
    repoRoot = tspath.NormalizePath(repoRoot)
    // ...thread repoRoot through handleRun to DiagnosticsToSARIF
}
```

### 2. Add repo-relative URI helper (`internal/luchta/sarif.go`)

```go
// Report paths relative to the worker launch directory (the repo root).
// Fall back to the absolute path for files outside the repo.
func repoRelativeSARIFURI(repoRoot, absPath string) string {
    rel, err := filepath.Rel(repoRoot, absPath)
    if err == nil && !filepath.IsAbs(rel) {
        relSlash := filepath.ToSlash(rel)
        if relSlash != ".." && !strings.HasPrefix(relSlash, "../") {
            return relSlash
        }
    }
    return filepath.ToSlash(absPath)
}

func DiagnosticsToSARIF(repoRoot string, diags []*ast.Diagnostic) string {
    // ...use repoRelativeSARIFURI(repoRoot, file.FileName())
}
```

**Guard-bug fix**: Normalized to forward slashes BEFORE checking `..` prefix. Naive `!strings.HasPrefix(rel, "..")` false-positives on legitimate in-repo dirs like `..config`. The correct check is `relSlash != ".." && !strings.HasPrefix(relSlash, "../")`.

### 3. Add unit tests (`internal/luchta/compile_test.go`)

```go
func TestRepoRelativeSARIFURI(t *testing.T) {
    tests := []struct {
        name    string
        absPath string
        want    string
    }{
        {"in repo strips root prefix", filepath.Join(repoRoot, "packages/app/src/foo.ts"), "packages/app/src/foo.ts"},
        {"outside repo falls back to full path", filepath.Join(outsideRoot, "src/foo.ts"), filepath.ToSlash(outsideRoot + "/src/foo.ts")},
        {"repo root itself returns dot", repoRoot, "."},
        {"dot dot prefix segment stays relative", filepath.Join(repoRoot, "..config/foo.ts"), "..config/foo.ts"},
    }
    // ...
}
```

### 4. Update hunk headers in patch

Each hunk header (`@@ -0,0 +1,N @@`) must match actual line count:

- `compile_test.go`: `@@ -0,0 +1,316 @@` (added test function)
- `sarif.go`: `@@ -0,0 +1,159 @@` (added helper function)
- `worker.go`: `@@ -0,0 +1,109 @@` (added repoRoot capture)

## Why This Works

1. **Launch-CWD convention**: The engine spawns workers with CWD pinned to repo root. `os.Getwd()` at `Serve()` startup captures this once; no `os.Chdir` calls exist, so it's stable for process lifetime.

2. **Path normalization**: `filepath.Rel` computes relative path from repo root to absolute file path. Forward-slash normalization (`filepath.ToSlash`) ensures portable SARIF URIs on Windows.

3. **Escape guard**: Checking `relSlash != ".." && !strings.HasPrefix(relSlash, "../")` prevents parent-directory escapes while allowing legitimate `..`-prefixed directory names like `..config`.

4. **Patch integrity**: Hunk headers matching actual line counts ensures `git apply` succeeds during `cargo xtask build-worker`.

## Prevention Strategies

**Testing the patch-embedded Go code:**

```bash
# Apply patch to submodule
cargo xtask build-worker

# Run Go tests directly
cd vendor/tsgo && go test ./internal/luchta/...

# IMPORTANT: Reset vendor/tsgo afterward to keep submodule pristine
cd vendor/tsgo && git reset --hard && git clean -fdx
```

**Code patterns to follow:**

- New workers MUST capture repo root at process start via `std::env::current_dir()` (Rust) or `os.Getwd()` (Go)
- SARIF `artifactLocation.uri` must use repo-relative paths via helpers like `repo_relative()` / `repoRelativeSARIFURI()`
- Paths outside repo fall back to absolute (forward-slashed) — matches Rust `strip_prefix().unwrap_or(path)` pattern

**Patch editing rules:**

- Edit only `+`-prefixed lines within `patches/tsgo.patch`
- Update `@@ -a,b +c,d @@` header line count to match new content
- Verify with `git -C vendor/tsgo apply --check ../../patches/tsgo.patch`

**Test coverage:**

- Unit test the path helper (not just integration-level SARIF output)
- Cover in-repo, outside-repo, root-equal, and `..`-prefixed directory cases
- Mirror test structure from Rust's `crates/luchta-worker/src/paths.rs`

## Related Issues

- **GitHub Issue:** [#259](https://github.com/dobesv/luchta/issues/259) — SARIF repo-relative paths for all workers
- **Plan:** `issue-259-sarif-repo-relative`
- **Related Solution:** [integration-issues/in-tree-go-worker-via-submodule-patch-2026-07-08.md](../integration-issues/in-tree-go-worker-via-submodule-patch-2026-07-08.md) — Patch-embedded Go worker build pattern
- **Design Doc:** `docs/superpowers/specs/2026-07-15-repo-root-relative-diagnostic-paths-design.md`
