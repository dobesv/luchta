---
title: "Unified ignore-aware file selection for oxfmt/oxlint workers with config-anchored patterns"
date: 2026-07-11
category: "logic-errors"
problem_type: logic_error
component: "luchta-oxfmt-worker, luchta-oxlint-worker"
root_cause: "config ignorePatterns resolved against task cwd instead of config file directory; WalkerBuilder incomplete without hard-skip filter; dual code paths (resolve + run) not audited for consistency"
resolution_type: code_fix
severity: high
tags:
  - ignore-patterns
  - walk-builder
  - monorepo
  - config-anchoring
  - cache-invalidation
plan_ref: "luchta-ignore-patterns"
---
# Problem

oxfmt and oxlint workers collected target files without honoring `.gitignore`, `.ignore`, tool-specific ignore files (`.oxfmtignore`/`.oxlintignore`), or config `ignorePatterns`. Build output directories like `dist/` were incorrectly formatted/linted. Moreover, when `ignorePatterns` were loaded from a parent config (e.g., workspace root `.oxlintrc`), they were resolved against the task's current working directory instead of the config file's directory, breaking monorepo scenarios.

# Symptoms

- `dist/`, `.next/`, and other build output directories were processed by workers despite being listed in `.gitignore`.
- Config `ignorePatterns` with anchored paths like `/src/` matched incorrectly in monorepos: pattern evaluated against package cwd instead of workspace root, causing wrong task-graph pruning.
- Changing `.gitignore` or `.ignore` did not invalidate cached worker results.
- Files incorrectly matched/excluded depending on user's global `~/.gitignore` settings (nondeterministic).

# Investigation Steps

1. Traced file collection in both workers. Found ad-hoc directory walks without ignore integration.
2. Identified `ignore::WalkBuilder` as unified solution with `.git_ignore(true)`, `.ignore(true)`, `.parents(true)`.
3. Added explicit `filter_entry` hard-skip for `node_modules`/`.git` — WalkBuilder alone does not guarantee exclusion when no `.gitignore` exists.
4. Discovered oxfmt correctly anchored `ignorePatterns` to config directory, but oxlint used task `cwd`. Reviewed `LintIgnoreMatcher::new(patterns, cwd, ...)` call.
5. Review found regression: `resolve_task` preflight path passed `cwd` as ignore base while `run_in_process` used `loaded.ignore_base`. Both paths MUST agree.
6. Added cache `inputs` entries for `.gitignore`, `.ignore`, tool-specific ignore files to ensure invalidation on ignore-rule changes.

# Root Cause

**WalkerBuilder without hard-skip**: WalkBuilder respects `.gitignore` when present, but without an explicit `filter_entry` skip, `node_modules` and `.git` could appear when no `.gitignore` exists in the tree.

**Config-anchoring mismatch**: `ignorePatterns` from a discovered `.oxlintrc`/`.oxfmt` config must resolve relative to the config file's parent directory (`ignore_base`), NOT the task cwd. Critical for monorepos where config lives at workspace root and workers run in nested package dirs.

**Dual code path inconsistency**: Worker has both a `resolve_task` (preflight, task-graph pruning) path AND a `run_in_process` (execution) path. Both call `collect_target_files` and BOTH must pass the same `ignore_base`. The execution path was fixed first; the resolve path initially kept using `cwd`, causing incorrect task pruning in nested runs.

**Cache invalidation gap**: File selection depends on ignore files, but `.gitignore`/`.ignore`/tool-ignore files were not declared as task `inputs`, so cached results survived ignore-rule changes.

# Solution

## 1. Unified WalkBuilder configuration

Both workers use `ignore::WalkBuilder` with deterministic flags:

```rust
WalkBuilder::new(root)
    .git_ignore(true)
    .ignore(true)
    .parents(true)
    .git_global(false)    // exclude user global git config
    .git_exclude(false)   // exclude .git/info/exclude
    .require_git(false)   // work outside git repos
    .follow_links(true)   // intentional: resolve symlinked dirs
    .filter_entry(|entry| {
        let path = entry.path();
        !path.file_name().map(|n| n == ".git" || n == "node_modules").unwrap_or(false)
    })
```

## 2. Hard-skip filter for node_modules/.git

```rust
.filter_entry(|entry| {
    let path = entry.path();
    let name = path.file_name();
    !name.map(|n| n == ".git" || n == "node_modules").unwrap_or(false)
})
```

## 3. Config-anchored ignore base

Config discovery returns the config file path. The matcher is built with `ignore_base` = config file's parent directory:

```rust
// In config.rs
let config_path = discover_config(...)?;
let ignore_base = config_path.parent().unwrap_or(&cwd).to_path_buf();

// In main.rs, BOTH paths must use the same base.
// oxlint contract: collect_target_files(cwd, ignore_patterns, ignore_base)
fn resolve_task(...) {
    let loaded = load_config(&cwd)?;
    let files = collect_target_files(&cwd, &loaded.ignore_patterns, &loaded.ignore_base);  // base = config dir, NOT cwd!
}

fn run_in_process(...) {
    let loaded = load_config(&cwd)?;
    let files = collect_target_files(&cwd, &loaded.ignore_patterns, &loaded.ignore_base);  // same base
}
```

For oxlint, using `LintIgnoreMatcher::new(patterns, ignore_base, ...)`.

## 4. Cache invalidation inputs

Worker task `inputs` must include ignore files:

```rust
let inputs = vec![
    // ... source files via glob or explicit
];
// Add ignore files that affect file selection
for name in [".gitignore", ".ignore", ".oxfmtignore"] {  // or .oxlintignore
    let ignore_file = cwd.join(name);
    if ignore_file.exists() {
        inputs.push(ignore_file);
    }
}
```

For WalkBuilder with `parents(true)`, ancestor ignore files also matter. Minimal approach: include task-cwd ignore files. Complete approach: also walk ancestors for `.gitignore`/`.ignore` files.

## 5. Regression test for anchoring

Test uses root config pattern `/src/` which matches DIFFERENTLY depending on base:

```rust
#[test]
fn resolve_task_anchors_parent_config_ignore_patterns_to_config_dir() {
    // Root .oxlintrc.json: { "ignorePatterns": ["/src/"] }
    // Package at packages/app with only src/foo.ts
    // 
    // Correctly anchored (config dir = root):
    //   /src/ matches <root>/src → package src/ survives → task KEPT
    //
    // Wrongly anchored (cwd = packages/app):
    //   /src/ matches <pkg>/src → source ignored → task PRUNED
    
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join(".oxlintrc.json"), r#"{"ignorePatterns":["/src/"]}"#).unwrap();
    
    let pkg = root.path().join("packages/app");
    fs::create_dir_all(pkg.join("src")).unwrap();
    fs::write(pkg.join("src/foo.ts"), "").unwrap();
    
    let result = resolve_task_from_cwd(&pkg);
    assert!(result.is_some());  // task kept when anchored correctly
}
```

# Why This Works

WalkBuilder centralizes ignore-file parsing and evaluation, matching git's behavior. The hard-skip filter guarantees `node_modules`/`.git` exclusion regardless of ignore-file presence. Config-anchored `ignore_base` ensures patterns evaluate relative to where they were defined, matching user expectation in monorepos. Dual-path consistency (`resolve` + `run`) prevents task-graph pruning from diverging from execution semantics. Cache inputs ensure ignore-rule changes invalidate results.

# Prevention Strategies

**Test Cases:**
- WalkBuilder collects files while respecting `.gitignore`, `.ignore`, parent `.gitignore`.
- Hard-skip filter excludes `node_modules`/`.git` even without `.gitignore`.
- Config `ignorePatterns` resolve from config file directory, not task cwd.
- Dual path test: resolve and run produce same file set for nested runs.
- Cache invalidation: change `.gitignore`, assert task re-runs.

**Best Practices:**
- When a config-derived value affects file selection, audit ALL call sites (preflight + execution).
- Add ignore files to task `inputs` when file selection depends on them.
- Use `require_git(false)` + explicit `git_global(false)`/`git_exclude(false)` for deterministic behavior.
- Prefer config-anchored patterns over cwd-anchored for inherited/shared configs.

**Code Review Checklist:**
- [ ] WalkBuilder flags use `git_global(false)` and `git_exclude(false)` for determinism.
- [ ] Hard-skip filter present for `node_modules`/`.git`.
- [ ] Config `ignorePatterns` resolve relative to config directory, not cwd.
- [ ] Both `resolve_task` (preflight) and `run_in_process` (execution) pass the same `ignore_base`.
- [ ] Ignore files declared in task `inputs` for cache invalidation.

# Related Issues

- **Plan:** [luchta-ignore-patterns](../../../plans/luchta-ignore-patterns.md)
- **Review:** [review-luchta-ignore-patterns](../../../plans/review-luchta-ignore-patterns.md) — Final report and review findings
