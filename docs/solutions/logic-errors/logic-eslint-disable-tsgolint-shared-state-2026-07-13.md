---
title: "eslint-disable comments apply to type-aware (tsgolint) diagnostics via shared disable-directives map"
date: 2026-07-13
category: logic-errors
problem_type: logic_error
component: luchta-oxlint-worker
root_cause: "tsgolint diagnostics filtered via separate should_skip_diagnostic path; disable_directives_map passed as empty because worker created fresh map instead of sharing LintService-internal map"
resolution_type: code_fix
severity: high
tags:
  - eslint-disable
  - tsgolint
  - type-aware-lint
  - oxc
  - arc-mutex
  - disable-directives
  - worker-phase
plan_ref: oxlint-config-eslint-disable
---

## Problem

`// eslint-disable` comments in source code suppressed standard lint diagnostics but had no effect on type-aware (tsgolint) diagnostics. The oxlint worker's type-aware path received an empty `disable_directives_map`, causing tsgolint's `should_skip_diagnostic` check to see no suppression directives and report errors that should have been suppressed.

## Symptoms

- Type-aware diagnostics like `@typescript-eslint/no-base-to-string` appeared in output despite `// eslint-disable-next-line` comments immediately before the offending line.
- Standard lint rules honored `// eslint-disable` as expected.
- Only visible when both `--type-aware` enabled and tsgolint executable available.

## Investigation Steps

1. Confirmed `// eslint-disable` worked for standard rules via `LintContext::add_diagnostic` which checks `disable_directives().contains(...)`.
2. Traced tsgolint path: `TsGoLintState::lint_source` collects diagnostics, then `tsgolint.rs::should_skip_diagnostic(&disable_directives_map, path, diag)` filters them.
3. Found `lint_files_blocking` created a fresh `Arc<Mutex<FxHashMap<PathBuf, DisableDirectives>>>` and passed it to `type_aware_linter.lint_source(...)`.
4. The standard lint pass runs via `service.run_source(...)` which populates the `LintService`'s *internal* disable directives map — not the fresh empty map passed to tsgolint.
5. The two maps were disconnected. Standard lint populated one; tsgolint read from an empty one.

## Root Cause

**Map lifecycle disconnect**: oxc (rev 415fe1e) stores disable directives discovered during `LintService::run_source` in an internal map. The worker passed a *different* empty map to tsgolint. The `should_skip_diagnostic` function uses `disable_directives_map.get(&path)` to check suppression — if the map is empty, no suppression applies.

The worker needed to:
1. Create ONE shared map.
2. Register it on `LintService` BEFORE `run_source` so it gets populated.
3. Pass the SAME map to tsgolint so it reads the populated data.

## Solution

```rust
let disable_directives_map: Arc<Mutex<FxHashMap<PathBuf, DisableDirectives>>> =
    Arc::new(Mutex::new(FxHashMap::default()));
service.set_disable_directives_map(disable_directives_map.clone());  // register BEFORE run

// Standard lint populates the map
let mut raw_messages = service.run_source(os_fs, paths.clone());

// tsgolint reads the SAME map (now populated)
if let Some(type_aware_linter) = &type_aware_linter {
    match type_aware_linter.lint_source(&paths, os_fs, disable_directives_map.clone()) {
        Ok(tsgo_messages) => raw_messages.extend(tsgo_messages),
        Err(error) => warnings.push(format!("type-aware lint failed: {error}")),
    }
}
```

Per-file ordering guarantees: `run_source` completes before `lint_source`, so directives for that file are always populated before tsgolint reads them.

## Why This Works

`set_disable_directives_map` registers an external map on the `LintService`. During `run_source`, the service populates this map with disable directives parsed from source comments. By passing the same `Arc` clone to both the service and the type-aware linter, both components share the same underlying data. The mutex provides safe interior mutability across the FFI boundary.

## Prevention Strategies

**Test Cases:**
- Integration test with type-aware rule, eslint-disable comment, and assertion that suppressed diagnostic is absent.
- Test fixture must have tsgolint available; gate with env var if needed.

**Code Review Checklist:**
- [ ] Disable-directives map created once and shared (not fresh each call).
- [ ] Map registered on LintService BEFORE `run_source`.
- [ ] Same map clone passed to tsgolint `lint_source`.
- [ ] Per-file ordering: standard lint completes before type-aware lint reads.

# Related Issues

- **GitHub:** [#221](https://github.com/dobesv/luchta/issues/221) — eslint-disable comments not applied to type-aware diagnostics
- **Related Solution:** [integration-issues/oxc-worker-in-process-integration-2026-07-08.md](../integration-issues/oxc-worker-in-process-integration-2026-07-08.md) — Original tsgolint integration design
