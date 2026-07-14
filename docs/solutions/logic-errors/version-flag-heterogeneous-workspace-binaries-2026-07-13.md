---
title: "Adding --version support across heterogeneous Rust workspace binaries"
date: 2026-07-13
category: "logic-errors"
problem_type: logic_error
component: "luchta-worker, luchta-cli, luchta-worker-watcher, all worker crates"
root_cause: "workspace binaries have 3 distinct entrypoint shapes; proxy argv partitioning must check only stage_args; feature-gated workers need ungated main() before gated real_main()"
resolution_type: code_fix
severity: medium
tags:
  - clap
  - argv
  - proxy-pattern
  - feature-gating
  - cfg-attribute
  - worker-binaries
  - version-flag
plan_ref: "version-flag-217"
---

## Problem

Workspace had 14 binaries without `--version`/`-V` support. Each binary falls into one of three shapes requiring different implementation approaches. Incorrect implementation caused: (1) proxies intercepting `--version` flags meant for delegate processes, (2) feature-gated workers failing to compile with `--no-default-features`.

## Symptoms

```
- `luchta --version` exited with error (flag not recognized)
- `luchta-worker-watcher -- node --version` printed watcher version instead of node's
- `cargo check --no-default-features -p luchta-oxc-transform-worker` failed with "cannot find function `version_requested` in this scope"
```

## Investigation Steps

1. Catalogued all 14 workspace binaries, identified 3 shapes:
   - clap-based (`luchta-cli`, `xtask`)
   - `run_worker_main`-based workers (stdin/stdout JSONL protocol)
   - proxy/filter binaries (forward args after `--` to delegate)
2. Clap binaries: trivial — add `#[command(version)]` to derive.
3. Workers: need version check before entering JSONL loop. Created shared helper `luchta_worker::version_requested`.
4. Proxies: discovered argv partitioning subtlety — `split_current_process_argv()` returns `.stage_args` (wrapper args before `--`) and `.delegate_command` (args after `--`). Must check ONLY `.stage_args` for wrapper `--version`.
5. Feature-gated workers (oxc, swc, oxfmt, oxlint): initial implementation gated the import of `version_requested` behind `#[cfg(feature)]`. Calling it from `#[cfg(not(feature))]` fallback main caused compile failure under `--no-default-features`.

## Root Cause

**Proxy argv conflation**: Using `std::env::args()` includes ALL args including delegate args after `--`. A `--version` meant for the delegate gets wrongly intercepted by the wrapper.

**Feature-gate cfg-visibility trap**: Importing a helper behind `#[cfg(feature = "oxc")]` makes it invisible to the `#[cfg(not(feature = "oxc"))`]` fallback main. The symbol exists in only one cfg variant; the other variant cannot see it.

**Unified main not possible with gated imports**: The fix requires a single UNGATED `fn main()` that performs version check first (via fully-qualified path or ungated import), then dispatches to a feature-gated `real_main()`.

## Solution

### 1. Clap binaries

Add `#[command(version)]` to the derive macro:

```rust
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli { ... }
```

### 2. Shared helper for non-clap binaries

```rust
// crates/luchta-worker/src/version.rs
pub fn version_requested(args: &[String], bin_name: &str, version: &str) -> bool {
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{bin_name} {version}");
        true
    } else {
        false
    }
}
```

Callers pass `env!("CARGO_PKG_NAME")` and `env!("CARGO_PKG_VERSION")` (workspace uses unified version).

### 3. Worker mains (non-gated)

```rust
fn main() {
    if luchta_worker::version_requested(
        &std::env::args().collect::<Vec<_>>(),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return;
    }
    run_worker_main(worker_impl);
}
```

### 4. Feature-gated workers

CRITICAL: Single ungated `main()`, feature-gated `real_main()`:

```rust
fn main() {
    // Version check MUST be ungated and use fully-qualified path
    if luchta_worker::version_requested(
        &std::env::args().collect::<Vec<_>>(),
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return;
    }
    real_main();
}

#[cfg(feature = "oxc")]
fn real_main() {
    run_worker_main(worker_impl);
}

#[cfg(not(feature = "oxc"))]
fn real_main() {
    eprintln!("this binary was built without the 'oxc' feature; worker unavailable");
    std::process::exit(1);
}
```

DO NOT gate the import of `version_requested`. Either use fully-qualified path or place an ungated `use` statement.

### 5. Proxy/filter binaries

Check ONLY `stage_args` from argv split:

```rust
fn async_main() -> i32 {
    let split = split_current_process_argv();
    
    // CRITICAL: Use split.stage_args, NOT std::env::args()
    if luchta_worker::version_requested(
        &split.stage_args,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    ) {
        return 0;  // early exit with success
    }
    
    // delegate_command contains args after '--'
    let delegate = split.delegate_command;
    // ... rest of proxy logic
}
```

Test case to verify correct behavior:

```rust
#[test]
fn version_flag_after_double_dash_stays_with_delegate() {
    let split = split_delegate_argv(
        ["wrapper", "--watch", "*.rs", "--", "node", "--version"].iter().map(|s| s.to_string())
    );
    assert!(!split.stage_args.contains(&"--version".to_string()));
    assert!(split.delegate_command.contains(&"--version".to_string()));
}
```

## Why This Works

- **Clap derive**: `#[command(version)]` auto-generates `--version`/`-V` handling with correct `<name> <version>` format.
- **Ungated main**: Version check runs regardless of feature cfg. Symbol visibility is not an issue because the check uses either fully-qualified path or ungated import.
- **Stage_args scope**: Proxy argv is cleanly partitioned at `--` boundary. Checking only `stage_args` ensures wrapper intercepts only its own flags, not flags meant for delegate.
- **Feature-gated dispatch**: Real worker logic lives in `real_main()` which IS gated. The fallback variant prints clear error and exits 1 for edge-case no-feature builds.

## Verification Checklist

For any workspace adding `--version` to binaries:

- [ ] Clap binaries: `#[command(version)]` present on derive
- [ ] Workers: `version_requested` called before entering JSONL loop
- [ ] Feature-gated workers: `main()` is UNGATED, `real_main()` is gated
- [ ] Feature-gated workers: NO gated imports used in ungated `main()` context
- [ ] Proxies: `version_requested` receives `split.stage_args`, NOT full argv
- [ ] Proxies: test case verifies `--version` after `--` stays in delegate_command
- [ ] **CRITICAL**: `cargo check --no-default-features -p <gated-crate>` passes for all feature-gated crates
- [ ] Manual: each binary `--version` outputs `<name> <version>`, exits 0

## Prevention Strategies

**CI coverage gap**: Default-feature builds hide feature-gate visibility bugs. Add CI step:

```yaml
- name: Check no-default-features for gated workers
  run: cargo check --no-default-features -p luchta-oxc-transform-worker -p luchta-oxfmt-worker -p luchta-oxlint-worker -p luchta-swc-transform-worker
```

**Binary shapes pattern**: When adding behavior to workspace binaries, first classify shape:
1. clap CLI — use derive attributes
2. Protocol-driven worker — check at entrypoint before protocol loop
3. Proxy/filter — check ONLY wrapper's own args (`stage_args`)

**Cfg-visibility discipline**: Symbols must be visible from ALL call sites. If a function is called from multiple cfg variants, either:
- Use fully-qualified path from an ungated context, OR
- Gate ONLY the implementation, not the import/declaration

## Related Issues

- **GitHub:** [dobesv/luchta#217](https://github.com/dobesv/luchta/issues/217) — Support --version on all binaries
- **Related Solution:** [integration-issues/oxc-worker-in-process-integration-2026-07-08.md](../integration-issues/oxc-worker-in-process-integration-2026-07-08.md) — Feature-gating patterns for oxc workers
