---
title: "In-process oxc worker integration with compile-validated API discovery"
date: 2026-07-08
category: integration-issues
problem_type: integration_issue
component: luchta-worker, luchta-oxlint-worker, luchta-oxc-transform-worker, luchta-oxfmt-worker
root_cause: "oxc git-pinned API churn; source-level API research divergence from pinned rev; private types in suppression API; feature-gating compile failures"
resolution_type: code_fix
severity: high
tags:
  - oxc
  - in-process-worker
  - git-dependency
  - msrv-bump
  - compile-validation
  - private-type-workaround
  - feature-gating
  - spawn-blocking
  - sarif
  - source-maps
  - config-discovery
  - type-aware-lint
plan_ref: oxc-workers
last_updated: 2026-07-08
---

## Problem

Adding in-process oxc-based resident workers to luchta required consuming oxc's git-pinned `main` branch crates with no stable releases. Source-level API research repeatedly diverged from the pinned revision, blocking implementation. The suppression API exposed an unnameable private type, and cargo feature declarations without proper cfg-gating broke `--no-default-features` builds.

## Symptoms

- **API research failures**: Librarian research claimed a `oxc_config_discovery` crate that didn't exist at pinned rev 415fe1e7. Signatures like `LintService::new(options, linter)` were wrong — actual order is `(linter, options)`. `ModuleRecord::new` at the pinned rev is allocator-only, not the documented `(path, &module_record, &semantic)`.
- **Private type blocker**: `SuppressionManager::build_diff()` returns `Arc<DiffManager>` where `DiffManager` is a private type. Cannot name it in struct fields, function signatures, or type aliases.
- **Feature-gate compile failure**: Declaring `default = ["oxc"]` with optional deps but no `#[cfg(feature = "oxc")]` guards caused `cargo check --no-default-features` to fail with 25+ unresolved import errors.
- **MSRV ripple**: oxc `main` requires Rust 1.94 + edition 2024, forcing workspace MSRV bump from 1.78 → 1.94, which surfaced 3 pre-existing clippy lints in unrelated crates via the newer toolchain.

## Investigation Steps

### 1. Compile-validated probe crates (methodology that unblocked everything)

Before implementing each worker phase, wrote throwaway `/tmp` probe crates pinned to exact oxc rev:

```rust
// /tmp/oxlint_service_probe/Cargo.toml
[dependencies]
oxc_linter = { git = "https://github.com/oxc-project/oxc.git", rev = "415fe1e7bb423cf05019c5e2c9a5705eebbc5447" }

// /tmp/oxlint_service_probe/src/main.rs
use oxc_linter::{LintService, LintServiceOptions, Linter, OsFileSystem};
// Compile to verify signatures actually work
```

This caught every API divergence before implementation. **Pattern**: research → probe crate → `cargo check` → only then implement.

### 2. Verified drivable lint path

Probe validated P2b path: `LintService::new(linter, options)` with `OsFileSystem`:

```rust
let config_store = ConfigStore::new(base_config, nested_configs, external_plugin_store);
let linter = Linter::new(LintOptions::default(), config_store, None);
let options = LintServiceOptions::new(cwd.into_boxed_path());
let service = LintService::new(linter, options); // ORDER MATTERS
let paths: Vec<Arc<OsStr>> = files.iter().map(|p| Arc::from(p.as_os_str())).collect();
let messages = service.run_source(&OsFileSystem, paths);
```

Low-level `Linter::run` with `ContextSubHost` and manual `ModuleRecord` construction is NOT needed.

### 3. Suppression manager private-type workaround

`SuppressionManager` is public but `DiffManager` is not. Solving constraint: keep `Arc<DiffManager>` as function-local `let`, never in struct field or signature:

```rust
let mut manager = SuppressionManager::load(cwd, "oxlint-suppressions.json", suppress_all, prune);
let diff = manager.build_diff(); // Arc<DiffManager> but type is inferred
let active: Vec<Message> = diff.collect_file(file, cwd, raw_messages);
// drop all other Arc refs before finalize
let (tx, _rx) = mpsc::channel();
manager.finalize(diff, &tx, cwd)?; // consumes Arc via Arc::into_inner
```

The whole load→collect→finalize lifecycle stays inside one function returning only nameable types.

### 4. Feature-gating fix

Declaring optional deps is insufficient. Source must gate:

```rust
// main.rs
#[cfg(feature = "oxc")]
mod config;
#[cfg(feature = "oxc")]
mod lint;

#[cfg(feature = "oxc")]
fn main() {
    // worker implementation
}

#[cfg(not(feature = "oxc"))]
fn main() {
    eprintln!("this binary was built without the 'oxc' feature; the worker is unavailable");
    std::process::exit(1);
}
```

Module files need `#![cfg(feature = "oxc")]` at top. Tests requiring oxc: `#[cfg(all(test, feature = "oxc"))]`.

## Root Cause

1. **oxc API churn**: `main` branch has no stability guarantee. Research based on source reading (even hours old) can diverge from pinned rev.
2. **Private suppression types**: oxc's suppression module (`suppression::RuntimeSuppressionMap`, `DiffManager`, `Filename`) is intentionally private. Only `SuppressionManager` and `OxlintSuppressionFileAction` re-exported.
3. **Feature-flag compile requirement**: Rust compiles all module files by default. Optional deps don't gate source; cfg attributes do.
4. **Dependency MSRV cascade**: oxc requires modern Rust. Workspace MSRV bump affects all crates, exposing latent lints.

## Solution

### In-process Worker trait extension

Added to `crates/luchta-worker/src/runtime.rs`:

```rust
pub trait Worker: Send + Sync + 'static {
    fn resolve_task(&self, req: &ResolveTask) -> ResolveResult;
    fn build_command(&self, req: &WorkerRequest) -> String;
    
    fn run_in_process(
        &self,
        _req: &WorkerRequest,
        _ctx: &JobContext,
    ) -> impl Future<Output = InProcessOutcome> + Send {
        async { InProcessOutcome::NotHandled }
    }
}

pub enum InProcessOutcome {
    NotHandled,
    Done { exit_code: i32, outputs: Option<Vec<String>> },
}
```

**Why `impl Future + Send` not `async fn`**: `tokio::JoinSet::spawn` requires the future be `Send`. Native `async fn` in trait does NOT guarantee `Send` bound on the returned future. Manual `impl Future<Output = ...> + Send` desugaring ensures it.

Runtime integration in `handle_request`:

```rust
// Try in-process first
let outcome = worker.run_in_process(req, &ctx).await;
match outcome {
    InProcessOutcome::Done { exit_code, outputs } => {
        done_with_outputs(writer, req.id.clone(), exit_code, outputs).await;
        return Ok(());
    }
    InProcessOutcome::NotHandled => { /* fall through to shell */ }
}
// Shell path for legacy workers...
```

### Git-pinning all oxc crates to single rev

Workspace `Cargo.toml`:

```toml
[workspace.dependencies]
oxc_linter = { git = "https://github.com/oxc-project/oxc.git", rev = "415fe1e7bb423cf05019c5e2c9a5705eebbc5447", optional = true }
oxc_parser = { git = "https://github.com/oxc-project/oxc.git", rev = "415fe1e7bb423cf05019c5e2c9a5705eebbc5447", optional = true }
# ... all oxc_* crates at same rev
```

Single rev ensures mutual AST/allocator compatibility. MSRV bump: `rust-version = "1.94.0"` with comment explaining oxc requirement.

### Hand-rolled SARIF (serde-sarif divergence)

Oxlint CLI hand-rolls SARIF; `serde-sarif` type names diverge. Worker mirrors:

```rust
#[derive(Serialize)]
struct SarifLog {
    version: &'static str,
    #[serde(rename = "$schema")]
    schema: &'static str,
    runs: Vec<SarifRun>,
}

#[derive(Serialize)]
struct SarifResult {
    ruleId: String,
    level: &'static str,
    message: SarifMessage,
    locations: Vec<SarifLocation>,
}
```

URIs repo-relative (strip `cwd` prefix), normalized with `/` separators. Levels: Error→"error", Warning→"warning", Advice→"note".

### Blocking work via spawn_blocking

All CPU-bound oxc work (lint/transform/format use Rayon internally) wrapped:

```rust
tokio::task::spawn_blocking(move || {
    // lint_files_blocking, transform_source, format_path
}).await.map_err(|e| format!("worker join error: {e}"))?
```

AGENTS.md excludes Rayon from luchta's own code; oxc's transitive Rayon is acceptable when isolated to blocking pool.

### Transform/Oxfmt API specifics

**Transform sequence**:
```rust
let allocator = Allocator::default();
let st = SourceType::from_path(path)?;
let program = Parser::new(&allocator, source, st).parse();
let sem_ret = SemanticBuilder::new().build(&program);
let scoping = sem_ret.semantic.into_scoping();
let options = TransformOptions::from_target(env_name)?;
let transformer = Transformer::new(&allocator, path, &options);
transformer.build_with_scoping(scoping, &mut program);
let code = Codegen::new().with_scoping(Some(scoping)).build(&program).code;
```

**Format sequence**:
```rust
// NOT Formatter::new — use free fn
let formatted = oxc_formatter::format(
    &allocator, source, SourceType::from_path(path)?,
    JsFormatOptions::new(), None
)?.print()?.into_code();
```

## Why This Works

1. **Compile-validated probes**: Eliminates API divergence before writing production code. Each probe catches signature drift in minutes rather than hours of debugging.

2. **Function-local private type constraint**: `DiffManager` exists only where needed. No struct field, no type alias, no leak across function boundary. Arc consumption via `finalize` is deterministic.

3. **Send-bound trait future**: Manual `impl Future + Send` satisfies tokio's `JoinSet::spawn` even though `async fn` in trait doesn't guarantee it.

4. **Feature-gate parity**: Source cfg-gates match Cargo.toml feature declarations. Disabled builds compile with clear error message.

5. **MSRV-aware clippy**: Bump surfaces dormant lints in unrelated code. CI on `dtolnay/rust-toolchain@stable` (≥1.95) absorbs the change.

## Prevention Strategies

### Test Cases

- **Probe-compile-first**: Before each new oxc API use, write scratch crate that compiles against pinned rev
- **Feature-gate verification**: Run `cargo check -p <oxc-worker> --no-default-features` in CI
- **Suppression byte-compat**: Compare `oxlint-suppressions.json` output byte-for-byte with CLI
- **SARIF shape parity**: Diff worker SARIF against oxlint CLI SARIF for same fixture

### Best Practices

- Pin ALL oxc_* crates to SAME git rev (mutual AST/allocator compat)
- Write probe crates for any git-dependency API surface before implementation
- Private types stay function-local; if type can't be named, can't be stored
- Feature declarations require matching cfg-gates — test with `--no-default-features`
- MSRV bumps surface latent toolchain lints — budget CI time for unrelated fixes

### Code Review Checklist

- [ ] All oxc_* crates pinned to single rev in workspace deps?
- [ ] API probe crate compiled before implementation?
- [ ] Private-type return values kept function-local?
- [ ] Feature-gated modules have `#[cfg(feature = "oxc")]`?
- [ ] Disabled-feature fallback provided?
- [ ] CPU-bound work wrapped in `spawn_blocking`?
- [ ] SARIF URIs normalized to `/` separators and cwd-relative?

## Related Issues

- **GitHub**: [dobesv/luchta#189](https://github.com/dobesv/luchta/issues/189) — Add oxc workers
- **Plan**: `oxc-workers` — Full implementation history in plan notes
- **Prior Art**: [worker-trait-harness-extraction-2026-06-11.md](./worker-trait-harness-extraction-2026-06-11.md) — Worker trait extension pattern
- **Prior Art**: [resident-worker-process-management-2026-06-09.md](./resident-worker-process-management-2026-06-09.md) — Exactly-one-Done invariant, spawn_blocking pattern

---

## Follow-up Features (Source Maps, .oxfmtrc, Type-Aware Lint)

The follow-up plan `oxc-workers-followups` extended the base implementation with three deferred features. Each required compile-validated API discovery and exposed non-obvious integration gotchas.

### 1. Transform Source Maps

**Requirement**: Emit `.js.map` files for transformed output, append `//# sourceMappingURL=` comment, report maps in outputs, clean up stale maps.

**API Discovery** (via `/tmp` probe crate):
```rust
// oxc_codegen::CodegenOptions
let options = CodegenOptions {
    source_map_path: Some(path), // triggers map generation
    ..Default::default()
};
let result = Codegen::new().with_options(options).build(&program);
// result.map: Option<oxc_sourcemap::SourceMap>
```

**Serialization**:
```rust
if let Some(map) = result.map {
    let json = map.to_json_string(); // JSON v3 format
    fs::write(&map_path, &json)?;
}
```

**Key Gotcha — oxc_sourcemap is NOT a git package**:
- `oxc_sourcemap` is a standalone crates.io crate (`oxc_sourcemap = "8"`), not a git dependency.
- Upstream workspace pins `oxc_sourcemap = "8.0.2"` in `Cargo.toml`.
- Cargo unified `8.0.2` (direct dep) and `oxc_codegen`'s transitive `8.1.0` to single version `8.1.0` (semver-compatible).
- Verified via `cargo tree` — no duplicate versions. Compile succeeded, confirming type compatibility.
- **Lesson**: Not all oxc crates are git-pinned. Check `Cargo.toml` pinning pattern per-crate. Registry deps unify with git-dep transitive if semver-compatible.

**Path handling**:
- `source_map_path` becomes the map's `sources[0]` field.
- Pass cwd-relative `/`-normalized path (not absolute) to avoid leaking build environment paths.
- Example: `src/nested/example.ts` rather than `/home/user/project/src/nested/example.ts`.

**Implementation** (`luchta-oxc-transform-worker/src/transform.rs`):
```rust
let source_map_path = relative_source_map_source_path(cwd, source_path);
let options = CodegenOptions {
    source_map_path: Some(source_map_path),
    ..Default::default()
};
// ... after codegen ...
if let Some(map) = result.map {
    let source_map_json = map.to_json_string();
    // Append sourceMappingURL comment to code
    code.push_str(&format!("\n//# sourceMappingURL={}.map\n", base_name));
}
```

**Cleanup**: Stale `.js.map` files removed via existing `cleanup_extra_files` (extension filter `Some("js" | "map")`).

### 2. Oxfmt .oxfmtrc Config Discovery

**Requirement**: Discover `.oxfmtrc.json` or `.oxfmtrc.jsonc` via ancestor walk, parse, and map Prettier-compatible subset to `JsFormatOptions`.

**Key Gotcha — oxfmt app crate NOT consumable as library**:
- `oxfmt` (app crate) has `default = ["napi"]` feature, pulling in `napi`, `napi-derive`, `tower-lsp-server`.
- At pinned rev `415fe1e7`, git vs registry allocator mismatch occurs:
  - `oxc_formatter_graphql` expects registry `oxc_graphql_parser::Allocator`.
  - Workspace uses git-pinned `oxc_allocator::Allocator`.
  - Build fails with type mismatch.
- Even `default-features = false` doesn't avoid the conflict — CSS/GraphQL formatters are unconditional deps.
- Config→options conversion (`to_oxc_formatter`) is `pub(crate)` — not accessible from external crate.

**Solution**: Hand-parse `.oxfmtrc.json(c)` and hand-map documented subset.

**Implementation** (`luchta-oxfmt-worker/src/config.rs`):
```rust
use json_strip_comments::StripComments;
use oxc_formatter::{
    BracketSameLine, BracketSpacing, JsFormatOptions,
    QuoteStyle, Semicolons, TrailingCommas,
};
use oxc_formatter_core::{IndentStyle, IndentWidth, LineEnding, LineWidth};

fn load_oxfmtrc(cwd: &Path) -> Result<JsFormatOptions, String> {
    let path = find_config_path(cwd)?; // ancestor walk
    let source = fs::read_to_string(path)?;
    // Strip comments for BOTH .json and .jsonc (Prettier ecosystem convention)
    let json = strip_json_comments(&source);
    let value: Value = serde_json::from_str(&json)?;
    oxfmtrc_to_options(&value)
}

fn oxfmtrc_to_options(v: &Value) -> Result<JsFormatOptions, String> {
    let mut opts = JsFormatOptions::new();
    if let Some(use_tabs) = v.get("useTabs").and_then(|v| v.as_bool()) {
        opts.indent_style = if use_tabs { IndentStyle::Tab } else { IndentStyle::Space };
    }
    // ... map 10 documented fields ...
    Ok(opts)
}
```

**Supported subset** (10 fields):
| Config field | `JsFormatOptions` field | Type/Enum |
|---|---|---|
| `useTabs` | `indent_style` | `IndentStyle::{Tab,Space}` |
| `tabWidth` | `indent_width` | `IndentWidth::try_from(u8)` |
| `printWidth` | `line_width` | `LineWidth::try_from(u16)` |
| `endOfLine` | `line_ending` | `LineEnding::{Lf,Crlf,Cr}` |
| `singleQuote` | `quote_style` | `QuoteStyle::{Single,Double}` |
| `jsxSingleQuote` | `jsx_quote_style` | `QuoteStyle::{Single,Double}` |
| `semi` | `semicolons` | `Semicolons::{Always,AsNeeded}` |
| `trailingComma` | `trailing_commas` | `TrailingCommas::{All,Es5,None}` |
| `bracketSpacing` | `bracket_spacing` | `BracketSpacing::from(bool)` |
| `bracketSameLine` | `bracket_same_line` | `BracketSameLine::from(bool)` |

**Enum/type sources**:
- `QuoteStyle`, `Semicolons`, `TrailingCommas`, `BracketSpacing`, `BracketSameLine` from `oxc_formatter::*`
- `IndentStyle`, `IndentWidth`, `LineEnding`, `LineWidth` from `oxc_formatter_core` (separate git crate)

**BracketSpacing/BridgeSameLine**: Opaque newtypes — access via `.value()` method or `From<bool>`.

**Comment stripping**: Apply to BOTH `.json` and `.jsonc` since Prettier ecosystem users often comment `.json` files.

**Unknown keys**: Silently ignored (serde default behavior). Tests verify graceful ignore.

### 3. Type-Aware Lint (tsgolint)

**Requirement**: Integrate `oxc_linter::TsGoLintState` for type-aware linting when `--type-aware`/`--type-check` enabled. Merge findings before suppression handling. Graceful skip when `oxlint-tsgolint` binary missing.

**API Discovery** (via `/tmp/ta3-tsgolint-probe`):
```rust
use oxc_linter::{
    ConfigStore, ConfigStoreBuilder, ExternalPluginStore,
    FixKind, Message, OsFileSystem, TsGoLintState,
};

let cwd = Path::new(".");
let config_store = ConfigStore::new(
    ConfigStoreBuilder::default().build(&mut external_plugin_store)?,
    FxHashMap::default(),
    external_plugin_store,
);

if config_store.type_aware_enabled() || opts.type_aware {
    match TsGoLintState::try_new(&cwd, config_store.clone(), FixKind::None) {
        Ok(tsgo) => {
            let tsgo = tsgo
                .with_silent(true)
                .with_type_check(config_store.type_check_enabled() || opts.type_check)
                .with_timings(false);
            let directives = Arc::new(Mutex::new(FxHashMap::default()));
            let tsgo_messages: Vec<Message> = tsgo.lint_source(
                &files,        // &[Arc<OsStr>]
                &OsFileSystem, // impl RuntimeFileSystem + Sync + Send
                directives,    // Arc<Mutex<FxHashMap<PathBuf, DisableDirectives>>>
            )?;
            raw_messages.extend(tsgo_messages);
        }
        Err(err) => {
            // Missing binary: warn and continue
            warnings.push(format!("type-aware lint unavailable: {err}"));
        }
    }
}
```

**Key Gotcha — oxlint spawns tsgolint internally**:
- `TsGoLintState` uses `std::process::Command` internally to spawn `oxlint-tsgolint` binary.
- No custom `ctx.spawn` or async runtime integration needed — this is purely a library-driven subprocess.
- The v1 assumption "needs ctx.spawn escape hatch" was wrong; compile-validated probe corrected this.

**Key Gotcha — `try_find_tsgolint_executable` NOT root-exported**:
- Function exists in `oxc_linter::tsgolint` module as `pub`.
- NOT re-exported from crate root (`lib.rs` does not `pub use`).
- External `use oxc_linter::try_find_tsgolint_executable` fails with "no X in root".
- Module is private, so cannot import `oxc_linter::tsgolint::try_find_tsgolint_executable`.
- **Solution**: Use `TsGoLintState::try_new` which internally calls finder. Returns `Err` if binary missing.

**Constructor choice**:
- `TsGoLintState::new(cwd, config_store, fix_kind)` — falls back to literal `"tsgolint"` string if not found, fails later at spawn.
- `TsGoLintState::try_new(...)` — returns `Err` immediately if binary not found.
- **Use `try_new`** for graceful skip pattern.

**Merge order** (critical):
```rust
let mut raw_messages = service.run_source(os_fs, paths.clone());
if let Some(tsgo) = &type_aware_linter {
    match tsgo.lint_source(&paths, os_fs, directives) {
        Ok(tsgo_messages) => raw_messages.extend(tsgo_messages),
        Err(e) => warnings.push(e),
    }
}
// THEN suppressions apply to combined messages
let active_messages = diff.collect_file(&file, &cwd, raw_messages);
```

Merge BEFORE `SuppressionManager` collection so type-aware findings respect `// eslint-disable` comments.

**FixKind decision**:
- `FixKind::None` used intentionally even with `--fix`.
- Plan note: "document later that type-aware autofix is intentionally not driven."
- Driving autofix through tsgolint in worker context would require non-public `DiffManager` and coordination with suppression management.

**Prerequisite**:
- `oxlint-tsgolint` is a USER-installed npm package (`npm i -D oxlint-tsgolint`).
- NOT distributed with luchta.
- Missing → warn to stderr, continue with regular lint.
- Never fails the worker for missing type-aware capability.

**Gating**:
```rust
let type_aware = opts.type_aware || config_store.type_aware_enabled();
let type_check = opts.type_check || config_store.type_check_enabled();
// type_check implies type_aware (upstream rejects type_check && !type_aware)
let type_aware = type_aware || type_check;
```

CLI flags added to `OXLINT_OPTS`: `--type-aware`, `--type-check`, hidden `--type-check-only`.

---

## Cross-Cutting Learnings from Follow-ups

### Compile-Validation Methodology Proved Critical Again

All three follow-ups required `/tmp` probe crates with `cargo check` against the pinned rev. Multiple assumptions were proven wrong:

1. **Source maps**: Assumed `oxc_sourcemap` was a git package. Probe revealed it's crates.io with version unification.
2. **Oxfmt config**: Assumed `oxfmt` app crate could be consumed as library. Probe showed git-vs-registry allocator conflict blocks it entirely.
3. **Tsgolint**: Assumed `try_find_tsgolint_executable` was root-exported (source reading). Probe showed it's NOT, and that spawn is internal to library.

**Lesson**: For git-pinned dependencies, source-level API research is insufficient. Compile-validated probes are mandatory before implementation.

### Feature-Gating Must Extend to All New Code

All three feature additions required:
- Cargo.toml: optional dependencies behind `oxc` feature
- Source files: `#![cfg(feature = "oxc")]` at module top
- Tests: `#[cfg(all(test, feature = "oxc"))]`

Compile verification: `cargo check -p <worker> --no-default-features` must pass for each crate.

### Registry+Git Dependency Unification Works for Semver-Compatible

When git-pinned crate depends on registry crate:
- Cargo unifies if semver-compatible.
- Types match (same crate, unified version).
- Verify with `cargo tree --duplicates` — should show single version per crate.

### Hand-Mapping Config Is Acceptable When Upstream Not Consumable

When upstream app crate has:
- Heavy dependency graph (napi, tokio, tower-lsp)
- Type mismatch (git vs registry allocator)
- Private conversion functions

Hand-mapping a documented subset to public library types is architecturally sound:
- Clear field mapping (straight-line assignment).
- Tests for each field.
- Integration test comparing output vs CLI catches drift.

### Library-Driven Subprocess Pattern

`tsgolint` integration demonstrated that:
- A library can internally spawn subprocesses without caller managing async runtime.
- `std::process::Command` in library code runs on library's own thread.
- Caller doesn't need special spawn handling — just call the method.

### Graceful Degradation for External Binaries

When feature depends on user-installed external binary:
- Use `try_new` / `Result`-returning constructor.
- On `Err`, emit warning and continue with reduced functionality.
- Never fail the entire worker for optional capability.
- Document prerequisite clearly in README.
