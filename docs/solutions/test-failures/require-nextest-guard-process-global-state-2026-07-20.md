---
title: "require_nextest() guard for process-global-state tests in Cargo workspace"
date: 2026-07-20
category: test-failures
problem_type: test_failure
component: luchta-test-support
root_cause: "Tests mutating process-global state (CWD, env vars, TempDir lifetimes) race under cargo test's shared-process threading model"
resolution_type: code_fix
severity: medium
tags:
  - nextest
  - test-isolation
  - process-global-state
  - cargo-workspace
  - flaky-tests
plan_ref: nextest-guard-252
---

## Problem

Tests that mutate or depend on process-global state (process CWD via `std::env::set_current_dir`, real environment variables via `set_var`/`remove_var`, or TempDir-lifetime races) race under plain `cargo test` (single process, shared threads) and fail nondeterministically. These tests pass under `cargo nextest run` because nextest runs each test in its own process, isolating global state.

## Symptoms

- `NotFound` panic in `luchta-cache` git tests when run under `cargo test`
- Intermittent test failures that disappear when run individually or single-threaded
- Tests pass reliably under `cargo nextest run --workspace` but flake under `cargo test`
- Error messages pointing to temp directory or file path issues

Example observed failure: TempDir lifetime race where one test's cleanup deleted another test's working directory mid-execution.

## Investigation Steps

Traced flaky failures to tests in `luchta-cache/src/shared/git.rs` that use `std::env::set_current_dir` and `tempfile::TempDir`. Under `cargo test`, all tests share a single process with multiple threads. Thread A changes CWD, Thread B expects its own CWD, causing races.

Reviewed other workspace crates for similar patterns. Found additional env-mutating tests in:
- `luchta-cli/src/run.rs`
- `luchta-cli/src/run/setup.rs` (missed initially, discovered during review)
- `luchta-cli/tests/cache_e2e.rs`, `no_cache_e2e.rs`, `cache_nonce_e2e.rs`
- `luchta-worker-watcher/src/watch.rs`

Confirmed that `cargo-nextest` sets `NEXTEST=1` in every test process, providing a reliable detection mechanism.

## Root Cause

`cargo test` runs all tests in a single process with configurable thread parallelism. Tests that mutate process-global state (CWD, environment variables) corrupt shared state for concurrent tests. `cargo-nextest` runs each test in its own OS process, providing complete isolation.

The `NotFound` panic occurred because TempDir drop in one thread deleted a directory that another thread's test was still using.

## Solution

Created workspace-internal crate `luchta-test-support` with a tiny guard function:

```rust
// crates/luchta-test-support/src/lib.rs
pub fn require_nextest() {
    if std::env::var("NEXTEST").is_err() {
        panic!(
            "This test requires cargo-nextest for process isolation.\n\
            Run with: cargo nextest run --workspace\n\
            See AGENTS.md for details."
        );
    }
}
```

Called as first line in each affected test:

```rust
#[test]
fn my_env_mutating_test() {
    require_nextest();  // Guard must be first line
    // ... test code that mutates process-global state
}
```

Added `[dev-dependencies]` entry to consuming crates:
```toml
[dev-dependencies]
luchta-test-support = { path = "../luchta-test-support" }
```

Documented `cargo nextest run --workspace` as canonical test command in `AGENTS.md` and `README.md`.

## Why This Works

`NEXTEST` environment variable is set by `cargo-nextest` in every test process. Plain `cargo test` does not set this variable. The guard panics early with actionable guidance before the test can corrupt shared state or race with other tests.

The `#[track_caller]` attribute on `require_nextest()` ensures panic messages point to the test call site, not the helper internals.

## Key Distinction

**Guard tests that mutate PROCESS-global state.** Do NOT guard tests that use:
- `std::process::Command::new(...).current_dir(path)` — sets CHILD process CWD, not parent
- `tempfile::TempDir` in tests that don't also use `set_current_dir` or env var mutation

A spawned child Command's `.current_dir(...)` is isolated to that child process and does NOT require the guard.

### How to Identify Affected Tests

Grep for:
- `std::env::set_var`
- `std::env::remove_var`
- `std::env::set_current_dir`

Used on the REAL process (not a spawned child Command). Also check TempDir-coupled tests that depend on CWD remaining stable.

## Pitfalls

**Tests extracted/moved to new modules require re-audit.** During review, 4 env-mutating tests in `luchta-cli/src/run/setup.rs` were initially missed because the module was new. When tests are refactored into new files, re-scan the new locations.

**Over-annotation is possible but harmless.** Some `git.rs` tests were annotated conservatively even though they use subprocess `.current_dir()` for the child. Better to be cautious than to miss a race.

## Deferred Enhancements

Out of scope for this issue, noted for future:
- Consolidate 5 duplicated `EnvVarGuard` helper implementations into `luchta-test-support`
- Add `CwdGuard` RAII helper for automatic CWD restoration
- Remove `ENV_LOCK` mutexes now redundant under nextest process isolation

## Related Issues

- **Issue:** [#252](https://github.com/dobesv/luchta/issues/252) — Tests: add require_nextest() guard for cwd/env/tempfile-sensitive tests
- **Related Solution:** [logic-errors/root-task-upstream-dependency-resolution-2026-07-15.md](../logic-errors/root-task-upstream-dependency-resolution-2026-07-15.md) — Documents nextest as canonical test runner
- **Related Solution:** [logic-errors/shared-build-cache-validate-on-restore-2026-06-17.md](../logic-errors/shared-build-cache-validate-on-restore-2026-06-17.md) — Test isolation gotcha with global mutable state
