---
title: "--config option support and command/env option source unification in oxlint worker"
date: 2026-07-13
category: integration-issues
problem_type: integration_issue
component: luchta-oxlint-worker
root_cause: "resolve_task parsed options from req.command only; run_in_process parsed from OXLINT_OPTS env only; disjoint sources caused phase mismatch"
resolution_type: code_fix
severity: high
tags:
  - oxlint
  - config-option
  - command-parsing
  - worker-phase
  - cache-correctness
  - quote-aware-tokenizer
  - ignore-patterns
plan_ref: oxlint-config-eslint-disable
---

## Problem

`--config <path>` option in task command was used during resolve/preflight phase but ignored during execution. Options were parsed from disjoint sources in each worker phase, causing cache correctness bugs and incorrect lint behavior. Additionally, `ignorePatterns` in a `--config` file anchor to the config file's parent directory, not the task cwd — a subtle but critical gotcha for tests and monorepos.

## Symptoms

- Task command `lint --config ./configs/strict.oxlintrc.json` discovered input files using the explicit config's `ignorePatterns` during resolve, but execution fell back to auto-discovered default config.
- Cache inputs computed from one config; execution used different rules → stale cache results.
- Suppression flags like `--suppress-all` in task command were ignored during resolution (passed `OxlintOpts::default()`).
- Config files in subdirectories had `ignorePatterns` that didn't match expected files because patterns anchored to config dir, not cwd.

## Investigation Steps

1. Traced `resolve_task` → `OxlintOpts::from_command(&req.command)` (command only).
2. Traced `run_in_process` → `OxlintOpts::from_request(req)` → parsed `req.env["OXLINT_OPTS"]` only.
3. Found `from_request` ignored `req.command` entirely; `from_command` ignored environment.
4. Documented `WorkerRequest` has both `command: String` and `env: HashMap<String, String>`.
5. Identified `ResolveTask` has `command` but no `env` field — asymmetry requires comment.
6. Tested `ignorePatterns` anchoring: config at `configs/strict.json` with pattern `*.test.ts` matched files relative to `configs/` dir, not cwd.

## Root Cause

**Phase mismatch**: Two worker phases parsed options from disjoint sources:
- `resolve_task` used `from_command` → task `command` field only
- `run_in_process` used `from_request` → `OXLINT_OPTS` env only

**Suppression discard**: `resolve_task` called `initial_suppression_action(cwd, &OxlintOpts::default())` after parsing opts, throwing away parsed values.

**Quote tokenizer bug**: Original `split_whitespace` tokenizer collapsed empty quoted strings (`""`), causing `--config "" --fix` to misparse.

**Ignore-pattern anchoring**: `ignorePatterns` in a config file resolve relative to config file's parent directory (`ignore_base`), NOT task cwd. Critical for monorepos and nested config files.

## Solution

### 1. Unified options parsing with merge semantics

```rust
impl OxlintOpts {
    pub fn from_request(req: &WorkerRequest) -> Self {
        let mut tokens = tokenize(&req.command);          // command first
        if let Some(raw) = req.env.get("OXLINT_OPTS") {
            tokens.extend(tokenize(raw));                  // then env
        }
        Self::parse_tokens(&tokens)                        // merged parse
    }

    pub fn from_command(command: &str) -> Self {
        Self::parse_tokens(&tokenize(command))            // resolve-task path
    }
}
```

Merge semantics:
- Boolean flags (`--fix`, `--suppress-all`): OR'd across both sources.
- `--config`: first non-empty value wins (command has priority).
- Empty config value (`--config ""`): treated as None.

### 2. Pass parsed opts to suppression action

```rust
// resolve_task
let opts = OxlintOpts::from_command(&req.command);
let action = initial_suppression_action(cwd, &opts);  // use parsed, not default
```

### 3. Quote-aware tokenizer preserving empty tokens

```rust
fn tokenize(raw: &str) -> Vec<String> {
    // Handles: --config '/path/with spaces' and --config="/path"
    // Preserves empty quoted tokens so --config "" --fix doesn't misparse
}
```

### 4. Config ignore-pattern anchoring

When `--config` specifies explicit config:
```rust
let ignore_base = config_path.parent().unwrap_or(&cwd).to_path_buf();
// ignorePatterns resolve relative to config file's directory
```

Test placement matters: config at cwd root → `ignore_base == cwd`. Config in `configs/` subdir → `ignore_base == configs/`, patterns match relative to that.

## Why This Works

Single source of truth (`from_request` merges both) ensures resolve and run agree on configuration. Command-first precedence matches user expectation: explicit task config overrides ambient env defaults. Quote-aware tokenizer handles paths with spaces and avoids empty-string collapse bugs. Config-anchored `ignore_base` matches how other tools (oxlint CLI, ESLint) resolve patterns.

## Prevention Strategies

**Test Cases:**
- `--config` in task command used in both resolve and run phases.
- `--config` in command wins over `OXLINT_OPTS` env config.
- Boolean flags OR'd when present in both.
- Empty quoted config value (`--config ""`) treated as None.
- Config in subdirectory: `ignorePatterns` match relative to config parent.

**Code Review Checklist:**
- [ ] Both worker phases use same options source (`from_request` merges command + env).
- [ ] Resolve path passes parsed opts to `initial_suppression_action`.
- [ ] `--config` file's `ignorePatterns` anchor to config parent, not cwd.
- [ ] Quote-aware tokenizer handles spaces and empty quotes.
- [ ] Integration test covers command-driven config (not just env).

# Related Issues

- **GitHub:** [#219](https://github.com/dobesv/luchta/issues/219) — Support `--config` option in oxlint task
- **Related Solution:** [logic-errors/ignore-pattern-worker-file-selection-2026-07-11.md](../logic-errors/ignore-pattern-worker-file-selection-2026-07-11.md) — Config-anchored ignore patterns
