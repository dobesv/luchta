---
title: "In-process ast-grep worker integration with probe-validated API at 0.43.0"
date: 2026-07-11
category: integration-issues
problem_type: integration_issue
component: luchta-ast-grep-worker
root_cause: "pre-1.0 crate API deviations from docs/spec; trait naming confusion; Severity trait bounds; column() signature"
resolution_type: code_fix
severity: high
tags:
  - ast-grep
  - in-process-worker
  - crates-io-dependency
  - compile-validation
  - spawn-blocking
  - sarif
  - config-discovery
  - severity-mapping
plan_ref: luchta-ast-grep-worker
---

## Problem

Integrating ast-grep library crates (0.43.0) for in-process linting required probe-validated API discovery because plan spec contained incorrect trait names and method signatures. Pre-1.0 crates lack stability guarantees; source-level research and docs diverge from published versions.

## Symptoms

```
- Error: `SupportLangExt` trait not found (plan spec claimed this trait existed)
- Error: `pos.column()` method signature mismatch — expected `()` args, actual `(&NodeMatch)`
- Error: `Severity` does not implement `Copy` or `PartialEq` — cannot derive on structs, cannot compare with `==`
- Compilation failures when attempting to store `Severity` in structs with derive macros
```

## Investigation Steps

Started with plan spec API notes. Multiple assumptions failed compilation:

1. **Trait import**: Plan claimed `SupportLangExt` trait for `from_path()`. Probe crate revealed it does NOT exist at 0.43.0. Correct trait is `LanguageExt` from `ast_grep_language`.
2. **Position extraction**: Plan implied `pos.column()` takes no arguments. Probe revealed `pos.column(&node_match)` requires `&NodeMatch`.
3. **Scan iteration**: Assumed named `ScanResult` type. Probe showed it's NOT re-exported — use type inference from `combined.scan()` return.
4. **Rule loading**: Assumed `from_yaml_string` returns `Vec` directly. Probe revealed `Result<Vec<RuleConfig<SupportLang>>, _>`.
5. **Severity traits**: Assumed `Severity` was `Copy + PartialEq`. Probe revealed neither trait implemented — `.clone()` required for storage, `matches!()` for comparison.

Created `/tmp` probe crate with `cargo check` against `=0.43.0` pins. Each probe validated one API surface before implementation.

## Root Cause

1. **Pre-1.0 API churn**: `ast-grep-*` crates at 0.43.0 have no semver stability guarantee. Published docs describe `main` branch which differs from released versions.
2. **Trait naming confusion**: `LanguageExt` vs `SupportLangExt` — easy to conflate when reading source without probe validation.
3. **Method signature deviations**: Position API differs from older docs; `column()` requires node reference.
4. **Incomplete trait bounds**: `Severity` enum lacks `Copy`/`PartialEq`, breaking common derive patterns.

## Solution

### Correct imports for 0.43.0

```rust
// src/lint.rs
use ast_grep_config::{from_yaml_string, CombinedScan, GlobalRules, RuleConfig, Severity};
use ast_grep_core::Language;
use ast_grep_language::{LanguageExt, SupportLang};
```

- `Language` from `ast_grep_core` for `lang.ast_grep(&source)`
- `LanguageExt` from `ast_grep_language` for `SupportLang::from_path(path)`

### Rule loading

```rust
pub fn load_rules(rule_files: &[PathBuf]) -> Result<Vec<RuleConfig<SupportLang>>, String> {
    let mut loaded = Vec::new();
    for rule_file in rule_files {
        let yaml = std::fs::read_to_string(rule_file)
            .map_err(|error| format!("failed to read {}: {error}", rule_file.display()))?;
        let mut rules = from_yaml_string(&yaml, &GlobalRules::default())
            .map_err(|error| format!("failed to load {}: {error}", rule_file.display()))?;
        loaded.append(&mut rules);
    }
    Ok(loaded)
}
```

Returns `Result<Vec<...>, _>`, not `Vec` directly.

### Scanning with type inference

```rust
let combined = CombinedScan::new(rules.iter().collect());
let result = combined.scan(&root, false);

for (rule_config, matches) in result.matches {
    for node_match in matches {
        let start = node_match.start_pos();
        let end = node_match.end_pos();
        findings.push(Finding {
            // ...
            severity: rule_config.severity.clone(), // NOT Copy
            start_column: start.column(&node_match) + 1, // requires &NodeMatch
            end_column: end.column(&node_match) + 1,
        });
    }
}
```

`ScanResult` not re-exported — iterate `result.matches: Vec<(&RuleConfig, Vec<NodeMatch>)>`.

### Severity handling

```rust
#[derive(Clone, Debug)] // NO PartialEq, NO Eq
pub struct Finding {
    pub severity: Severity,
    // ...
}

// To check for error severity:
if matches!(finding.severity, Severity::Error) { ... }

// To map to SARIF level:
fn map_level(sev: &Severity) -> &'static str {
    match sev {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "note",
        Severity::Hint => "note",
        Severity::Off => "none", // extra variant at 0.43.0
    }
}
```

`Severity` has extra `Off` variant — exhaustiveness requires handling.

### In-process execution pattern

```rust
pub async fn scan_files_async(
    cwd: &Path,
    rule_files: &[PathBuf],
    files: Vec<PathBuf>,
) -> Result<Vec<Finding>, String> {
    let cwd = cwd.to_path_buf();
    let rule_files = rule_files.to_vec();
    tokio::task::spawn_blocking(move || scan_files(&cwd, &rule_files, files))
        .await
        .map_err(|error| format!("ast-grep worker join error: {error}"))?
}
```

CPU-bound scanning via `spawn_blocking`. Same pattern as `luchta-oxlint-worker`.

### Worker trait implementation

```rust
impl Worker for AstGrepWorker {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult { ... }

    fn build_command(&self, _req: &WorkerRequest) -> String {
        String::new() // in-process worker — command never called
    }

    // Manual async form for Send bound
    #[allow(clippy::manual_async_fn)]
    fn run_in_process(
        &self,
        req: &WorkerRequest,
        ctx: &JobContext,
    ) -> impl Future<Output = InProcessOutcome> + Send {
        async move {
            // ... load config, scan, emit SARIF
        }
    }
}
```

`impl Future<Output = ...> + Send` form required for `tokio::JoinSet::spawn` compatibility.

### Config discovery

```rust
// Walk Path::ancestors() for sgconfig.yml
for dir in cwd.ancestors() {
    let config_path = dir.join("sgconfig.yml");
    if config_path.exists() {
        // Parse ruleDirs with serde_norway
        let config: SgConfigFile = serde_norway::from_str(&contents)?;
        // Collect .yml/.yaml files from ruleDirs
    }
}
```

Prune task if `sgconfig.yml` absent or `rule_files` empty.

### Cache inputs for per-package tasks

Ancestor discovery stays enabled, but worker-owned declared inputs must stay package-relative. If
`sgconfig.yml` or resolved rule files live above task `cwd`, worker omits them from its generated
`TaskModification.inputs` instead of emitting `../...` paths.

Engine semantics matter here: worker `inputs` modifications replace task inputs rather than merge
with them. To keep shared-root cache invalidation working, consumer `luchta-config.*` should
declare shared root config/rules as repo-root `#`-prefixed inputs, and the ast-grep worker must
preserve those `#...` entries across resolve while adding its package-relative inputs.

### SARIF construction

```rust
// Line/column from library are 0-based; SARIF requires 1-based
start_line: start.line() + 1,
start_column: start.column(&node_match) + 1,

// Emit only when findings non-empty
if !findings.is_empty() {
    ctx.emit_report("ast-grep.sarif", "application/sarif+json", sarif).await?;
}

// Exit code: 1 if any Error severity, 0 otherwise
let exit_code = if findings.iter().any(|f| matches!(f.severity, Severity::Error)) {
    1
} else {
    0
};
```

## Why This Works

1. **Probe validation before implementation**: Each API assumption verified against pinned version. Prevents divergence from plan spec errors.
2. **Exact version pinning**: `=0.43.0` prevents semver-compatible updates breaking the build.
3. **Type inference for unexported types**: `ScanResult` internal; iterating `result.matches` works without naming it.
4. **Clone over derive**: `Severity` lacks `Copy` — `.clone()` in struct construction, `matches!()` for comparisons.
5. **spawn_blocking for CPU work**: Keeps tokio runtime responsive while scanning.

## Prevention Strategies

**Test Cases:**
- Unit test: load rules from YAML, verify `RuleConfig<SupportLang>` returned
- Unit test: scan trivial source with trivial rule, verify finding position
- Integration test: `run_in_process` with temp dir containing `sgconfig.yml` + rule file
- Integration test: verify SARIF output shape matches schema
- Integration test: exit code 1 when `Severity::Error` present

**Best Practices:**
- Create `/tmp` probe crate with `cargo check` for ANY pre-1.0 dependency API
- Pin exact versions: `=0.X.Y` not `0.X`
- Use `spawn_blocking` for CPU-bound linting, never block async runtime
- Check for extra enum variants when matching on library types
- Document API deviations in code comments for future maintainers

**Code Review Checklist:**
- [ ] All imports verified against pinned version (not main branch docs)
- [ ] Position methods use correct signatures (`column(&node_match)`)
- [ ] Severity stored with `.clone()`, compared with `matches!()`
- [ ] `run_in_process` returns `impl Future<Output=...> + Send`
- [ ] SARIF line/column are 1-based (library is 0-based)

**Compile-Probe Template:**
```toml
# /tmp/ast-grep-probe/Cargo.toml
[package]
name = "ast-grep-probe"
version = "0.1.0"
edition = "2021"

[dependencies]
ast-grep-core = "=0.43.0"
ast-grep-config = "=0.43.0"
ast-grep-language = "=0.43.0"
```

```rust
// src/lib.rs — probe each API assumption
use ast_grep_core::Language;
use ast_grep_language::{LanguageExt, SupportLang};

pub fn probe_from_path(path: &std::path::Path) -> Option<SupportLang> {
    SupportLang::from_path(path) // validates trait import
}
```

Run `cargo check` — fix errors before writing worker code.

## Related Issues

- **Issue:** [#207](https://github.com/example/luchta/issues/207) — Add ast-grep worker
- **Commit:** `6e1db3fa2d` — Add luchta-ast-grep-worker in-process worker
- **Related Solution:** [oxc-worker-in-process-integration-2026-07-08.md](./oxc-worker-in-process-integration-2026-07-08.md) — Compile-probe methodology pattern established
