---
title: "In-process SWC worker integration with swcrc discovery and thread-local globals"
date: 2026-07-08
category: integration-issues
problem_type: integration_issue
component: luchta-swc-transform-worker
root_cause: "swc_core feature-gated modules; .swcrc target override semantics; thread-local GLOBALS; cache-invalidation for ancestor config"
resolution_type: code_fix
severity: high
tags:
  - swc
  - in-process-worker
  - swcrc
  - globals
  - spawn-blocking
  - source-maps
  - feature-gating
  - compile-validation
  - cache-invalidation
plan_ref: swc-transform-worker
---

## Problem

First SWC integration in the repo. Required compile-validated API discovery against crates.io `swc_core =73.0.0`, `.swcrc` compatibility with correct target-override semantics, thread-local `GLOBALS` scope management inside `spawn_blocking`, and cache-invalidation for ancestor `.swcrc` files crawled by SWC's config discovery.

## Symptoms

- `features=["base"]` alone failed compilation: `swc_core::common` and `swc_core::ecma` modules are feature-gated separately.
- `.swcrc` target not honored when programmatic `jsc.target: Some(...)` passed — `.swcrc` target never overrode programmatic value.
- SWC APIs diverged from naive docs: `TsConfig` renamed to `TsSyntax`, `TransformOutput.map` field, `try_with_handler` flattens inner `Result`.
- Cache invalidation gap: `find_swcrc` crawls up ancestor dirs, but initial `resolve_task` only tracked package-local `.swcrc`, missing parent/root configs.
- Source maps written with absolute `sources` paths; needed repo-relative for build artifact portability.
- Input grammar rejects `../` and absolute paths; ancestor config tracking requires workspace-root-relative patterns.

## Investigation Steps

1. **MANDATORY probe crate first**: Per oxc post-mortem, created `/tmp/swc-probe` to compile-validate `swc_core = "=73.0.0"` before writing worker code. This caught multiple API drift issues.
2. **Feature flag discovery**: Started with `features=["base"]` alone — compile failed. Added `common`, `ecma_ast`, `ecma_parser` one by one until imports resolved.
3. **`.swcrc` override semantics**: Probe tested `.swcrc` with `"jsc":{"target":"es5"}` while passing programmatic `target: Some(EsVersion::Es2022)`. Result: `es5` output NOT produced. Retested with `target: None` — `.swcrc` target honored.
4. **Thread-safety verification**: Confirmed `GLOBALS.set` + `try_with_handler` must run inside single `spawn_blocking` closure; fresh `SourceMap`/`Globals` per file.
5. **Cache semantics audit**: Traced input expansion (`luchta-engine/src/input_expansion.rs`), found `../` rejected, root-relative patterns via `#` prefix. `ResolveTask` has no workspace root access.
6. **Source map post-processing**: Verified SWC writes absolute filename in `sources`; wrote serde_json rewrite to repo-relative path.

## Root Cause

### Feature Gates

`swc_core` uses feature flags to gate public modules. `base` facade pulls internal dependencies but does NOT expose `common`/`ecma_*` module types at crate root. Each publicly-used module requires its corresponding feature.

### Target Override Semantics

`Compiler::process_js_file` merges `.swcrc` over programmatic `Options`, but `jsc.target` follows specific merge rules: if programmatic `target: Some(...)`, that wins — `.swcrc` target does NOT override. To honor `.swcrc` target, must pass `target: None` when `.swcrc` exists.

### Thread-Local Globals

SWC uses `GLOBALS` thread-local for AST intern IDs. The entire pipeline must run within `GLOBALS.set(&globals, || ...)`. Crossing `.await` with SWC AST/SourceMap causes undefined behavior or panics.

### Ancestor Config Cache Invalidation

`find_swcrc` crawls `abs_path.parent().ancestors()` checking for `.swcrc`. SWC may find config up the tree. `ResolveTask` only receives `cwd` (package dir), not workspace root. Input grammar rejects `../` path escapes. Solution: root-qualified workspace glob `#**/.swcrc` as input.

## Solution

### Pinned Dependency with Correct Features

**Cargo.toml**:
```toml
[dependencies]
swc_core = { workspace = true, optional = true }

[features]
default = ["swc"]
swc = ["dep:swc_core"]
```

**Workspace `Cargo.toml`** (exact pin):
```toml
[workspace.dependencies]
swc_core = { version = "=73.0.0", features = ["base", "common", "ecma_ast", "ecma_parser"] }
```

**Why exact pin**: SWC breaks public API on minor bumps. MSRV 1.94 already satisfied.

### `.swcrc` Compatibility via High-Level Facade

**Options construction** (target semantics):
```rust
let has_swcrc = find_swcrc(abs_path); // crawl ancestors

let opts = Options {
    config: Config {
        jsc: JscConfig {
            syntax: Some(Syntax::Typescript(TsSyntax {
                tsx: true,
                ..Default::default()
            })),
            target: resolve_target(has_swcrc), // None if .swcrc, else Some(Es2022)
            ..Default::default()
        },
        ..Default::default()
    },
    filename: abs_path.to_string_lossy().into_owned(),
    swcrc: true, // enable .swcrc discovery
    source_maps: Some(SourceMapsConfig::Bool(true)),
    ..Default::default()
};
```

**resolve_target helper**:
```rust
fn resolve_target(has_swcrc: bool) -> Option<EsVersion> {
    if has_swcrc {
        None // let .swcrc target win
    } else {
        Some(EsVersion::Es2022) // hard-coded default
    }
}
```

**find_swcrc crawl**:
```rust
fn find_swcrc(abs_path: &Path) -> bool {
    abs_path
        .parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .any(|dir| dir.join(".swcrc").is_file())
}
```

### Thread-Local GLOBALS + spawn_blocking

**Working wrapper**:
```rust
fn run_transform(
    compiler: Compiler,
    cm: Lrc<SourceMap>,
    fm: Lrc<swc_core::common::SourceFile>,
    opts: &Options,
) -> Result<TransformOutput, Vec<String>> {
    let globals = Globals::default();
    GLOBALS
        .set(&globals, || {
            try_with_handler(
                cm,
                HandlerOpts {
                    color: ColorConfig::Never,
                    ..Default::default()
                },
                |handler| compiler.process_js_file(fm, handler, opts),
            )
        })
        .map_err(|error| vec![error.to_string()])
}
```

**Worker call site** (inside `spawn_blocking`):
```rust
let source_path_for_task = source_path.clone();
let result = task::spawn_blocking(move || {
    transform_source(
        &source_path_for_task,
        &source,
        &source_map_source_path,
        &source_mapping_url,
    )
})
.await;
```

Fresh `SourceMap` + `Globals` per file. NEVER hold SWC AST/SourceMap across `.await`.

### Source Map Post-Processing

SWC writes absolute filename in `sources`. Rewrite to repo-relative:
```rust
fn rewrite_source_map_sources(
    source_map_json: &str,
    source_map_source_path: &Path,
) -> Result<String, Vec<String>> {
    let mut source_map: Value = serde_json::from_str(source_map_json)
        .map_err(|error| vec![error.to_string()])?;
    let normalized_source = normalize_path(source_map_source_path);
    source_map
        .as_object_mut()
        .ok_or_else(|| vec!["source map json should be an object".into()])?
        .insert("sources".to_owned(), serde_json::json!([normalized_source]));
    serde_json::to_string(&source_map).map_err(|error| vec![error.to_string()])
}
```

### Cache Invalidation for Ancestor Config

`resolve_task` inputs:
```rust
let mut inputs = BTreeSet::from([
    "package.json".to_owned(),
    "src/**".to_owned(),
    ".swcrc".to_owned(),
    "#**/.swcrc".to_owned(), // workspace glob for ancestor configs
]);
```

**Why workspace glob**: `ResolveTask` has no workspace root access. Input grammar rejects `../`. Root-qualified glob catches any ancestor `.swcrc` creation/change. Over-invalidates other packages by design — conservative but correct.

### Feature-Gating Checklist

1. **Cargo.toml**: optional dep behind feature
2. **Module header**: `#![cfg(feature = "swc")]`
3. **Tests**: `#[cfg(all(test, feature = "swc"))]`
4. **Fallback main**:
```rust
#[cfg(not(feature = "swc"))]
fn main() {
    eprintln!("swc feature disabled; worker not available");
    std::process::exit(1);
}
```
5. **Verify**: `cargo check -p luchta-swc-transform-worker --no-default-features`

### Verified Imports

```rust
use swc_core::base::config::{Config, JscConfig, Options, SourceMapsConfig};
use swc_core::base::{try_with_handler, Compiler, HandlerOpts, TransformOutput};
use swc_core::common::errors::ColorConfig;
use swc_core::common::sync::Lrc;
use swc_core::common::{FileName, Globals, SourceMap, GLOBALS};
use swc_core::ecma::ast::EsVersion;
use swc_core::ecma::parser::{Syntax, TsSyntax};
```

## Why This Works

- **Feature flags**: Each public `swc_core` module (`common`, `ecma_ast`, `ecma_parser`) is behind its own feature gate. `base` alone doesn't expose module-level types.
- **Target None for .swcrc**: SWC merge semantics: programmatic `target: Some` wins over `.swcrc`. Setting `None` when `.swcrc` exists allows `.swcrc` target to apply.
- **GLOBALS.set inside spawn_blocking**: Thread-local validity scoped to closure; `spawn_blocking` ensures single-threaded execution on blocking thread pool.
- **Workspace glob**: Input grammar only allows package-relative or workspace-root-relative (`#`). Ancestor tracing without root access requires workspace-wide pattern.
- **Exact version pin**: API stability across SWC minor releases is not guaranteed.

## Prevention Strategies

### MANDATORY: Probe Churny Dependencies First

For any dependency with API churn history (SWC, OXC, etc.):

1. Create `/tmp/<name>-probe` crate with exact pinned version
2. Compile-validate every import before writing production code
3. Test actual behavior (not just compile) for subtle semantics

This caught: `TsSyntax` rename, `.swcrc` target override rules, `try_with_handler` result flattening.

### Feature-Gating Checklist

Before marking worker complete:

- [ ] Cargo.toml: optional dep behind feature
- [ ] Every source file using gated dep: `#![cfg(feature = "...")]`
- [ ] All tests: `#[cfg(all(test, feature = "..."))]`
- [ ] Fallback when disabled
- [ ] `cargo check -p <crate> --no-default-features` passes

### Ancestor Config Cache Invalidation

For any worker whose tool crawls ancestor config:

1. Identify config file name(s) (`.swcrc`, `.oxlint.json`, etc.)
2. Add workspace root glob input: `#**/.<configname>`
3. Document over-invalidation tradeoff (all packages invalidate on any config change)

### Thread-Local Globals Pattern

When integrating threaded native code:

1. Entire pipeline inside `GLOBALS.set` closure
2. Closure runs within `spawn_blocking`
3. Fresh context per file (no cross-file state)
4. Never hold AST/SourceMap across `.await`

## Related Issues

- **GitHub**: [#192](https://github.com/dobesv/luchta/issues/192) — Add swc worker
- **Prior Art**: [oxc-worker-in-process-integration-2026-07-08.md](./oxc-worker-in-process-integration-2026-07-08.md) — MANDATES compile-validated probes; `spawn_blocking` pattern; feature-gating checklist; `impl Future + Send` trait method.

---

## Phase 2: CLI-configurable transform (SWC-CLI parity)

### Problem

Phase-1 worker only read `.swcrc`. Consumer (app-luchta) needed programmatic configuration: preset-env/core-js, per-package module type, JSX automatic runtime, dual browser/node builds, shared central config at workspace root. Required SWC-CLI-style flags parsed from task command string.

### Solution: JSON-to-Options Bridge

Parse SWC-CLI flags from `WorkerRequest.command`:

```
--no-swcrc           → Options.swcrc = false
--config-file <path> → Options.config_file
--env-name <name>    → Options.env_name (NOT babel-style env block selection)
-C key=value (×N)    → deep-merge into serde_json::Value, JSON-coerce "true"/numbers
-d/--out-dir <dir>   → CLI-level output directory (replaces fixed dist/js)
--source-maps <v>    → Options.source_maps (true/inline)
```

**Bridge mechanism** (pragmatic for SWC's churny typed config):

```rust
// 1. Base config: TS parser + JSX
let mut config = serde_json::json!({
    "jsc": {
        "parser": { "syntax": "typescript", "tsx": true }
    }
});

// 2. Deep-merge -C overrides (JSON-coerce "true"→bool, "123"→number)
merge_overrides(&mut config, parse_dash_c_flags(command));

// 3. Deserialize to typed Options
let opts: Options = serde_json::from_value(config)?;

// 4. Post-fixups (see Gotchas below)
```

### Feature Flag Correction

**Probe-only catch**: `module.type=commonjs` silently NO-OPs (emits ESM) unless `base_module` feature enabled.

```toml
# WRONG (Phase 1): compiles but wrong output
features = ["base", "common", "ecma_ast", "ecma_parser"]

# CORRECT (Phase 2): actual CJS transform works
features = ["base_module", "common"]
```

`base_module` implies `__base` + enables real module transform. Old `["base", ...]` compiles fine but module config deserializes to stub and transform returns unchanged ESM.

### Two Hard Runtime Gotchas (Probe-Verified)

1. **`env` (preset-env) and `jsc.target` are MUTUALLY EXCLUSIVE**:
   ```
   runtime error: "`env` and `jsc.target` cannot be used together"
   ```
   Worker must drop `jsc.target` when top-level `env` is present.

2. **`sourceMaps: "both"` PANICS in `process_js_file`**:
   ```
   assertion failed: Source map must be true, false or inline
   ```
   Normalize `inline`/`both` → `true` (external maps). Worker emits `.js.map` regardless.

### envName Limitation

`Options.env_name` does **NOT** select `.swcrc` `env: { dev: {...}, prod: {...} }` blocks in swc_core 73. Per-env config must come from distinct per-task flags (`-C`, `--config-file`, `--out-dir`), not env-block selection.

### Workspace-Root Config Resolution (Reusable Pattern)

Worker process runs with `cwd = workspace root`. `req.cwd` values are workspace-root-relative (e.g., `"packages/foo"`).

**Resolution rule for `--config-file`**:
- `--config-file swc.node.json` (relative) → resolve against `current_dir()` (workspace root)
- `--config-file '#swc.node.json'` → strip `#`, resolve against workspace root
- Package-relative not needed; shared configs sit at root

**Cache input**: emit the exact workspace-root-relative path as a `#`-qualified input (e.g., `#swc.node.json`). Engine grammar `#path` = root. Precise invalidation — replaces conservative `#**/<basename>` glob.

```rust
// When config-file is outside req.cwd (package dir):
let input = format!("#{}", config_relpath_from_root);
```

**Reusable for any worker needing shared central config**: resolve against `current_dir()`, emit as `#path` input.

### Consumer Integration Shape

Reference: app-luchta `luchta-config.mts`:

```typescript
// Gate on env var
if (process.env.LOCAL_TRANSPILER === 'swc') {
  // Route build:<env> to swc worker
  worker: 'swc-transform',
  command: [
    '--no-swcrc',
    `--env-name ${env}`,
    `--out-dir dist/${env}`,
    `--config-file '#swc.${env}.json'`,
    `-C module.type=${moduleType}`  // es6 or commonjs
  ].join(' ')
}

// CJS packages' node build → babel (SWC CJS output not cjs-module-lexer-readable)
if (pkgJson.type !== 'module' && env === 'node') {
  worker: 'babel-transform'  // fallback
}
```

**Shared config (`swc.<env>.json`)**:
- Strict JSON (no comments — worker parses via `serde_json`)
- Uses `env.targets` (browserslist/node), NOT `jsc.target` (exclusivity)
- Example: `{"env":{"mode":"entry","coreJs":"3.30","targets":"> 0.25%"}}`

**Deferred**:
- WASM plugins (styled-components): needs `--config-file` with `jsc.experimental.plugins` (arrays can't go via `-C`)
- CSS import stripping: moving to package.json import maps

### Prevention Additions

- **Module feature catch**: When config exposes module transform, compile-validate probe must test **actual output**, not just deserialization
- **Runtime exclusivity check**: For any library with mutually-exclusive config fields, add guard in merge layer
- **Panic avoidance**: Normalize config values that compile but panic at runtime (probe runtime tests, not just serde)

## Consumer Integration + Review Learnings (2026-07-10)

### @swc/core NPM Binary vs swc_core Crate DIVERGE on rewriteRelativeImportExtensions

HIGHEST-VALUE, NON-OBVIOUS: the Rust `swc_core` crate (v73) and the `@swc/core` NPM binding (tested 1.15.40 AND 1.15.43) behave DIFFERENTLY for `jsc.rewriteRelativeImportExtensions: true`:

**Rust swc_core (correct):**
```rust
// swc_ecma_transforms_module 51.0.0, import_rewriter_typescript.rs
"tsx" => if jsx_preserve { "jsx" } else { "js" }
// jsx_preserve = react.runtime == Some(Preserve)
// runtime=automatic → jsx_preserve=false → .tsx→.js ✅
```

Worker (Rust, via `Compiler::process_js_file` with `.swcrc`) emits `.tsx`→`.js` correctly for `react.runtime: automatic`.

**@swc/core NPM binary (buggy):**
Static `.tsx` always rewrites to `.jsx` regardless of `react.runtime`. Tested exhaustive config matrix:
- `runtime: automatic/classic/preserve/omitted`
- `jsc.parser.tsx: true/false`
- `module.type: commonjs/es6`
- `filename: .tsx/.ts`
- `swcrc: false`, `configFile: false`
- All combinations → `.tsx`→`.jsx`. No programmatic config flips it.

**Evidence:**
- Empirical probe: `@swc/core@latest` (1.15.43) still exhibits `.tsx`→`.jsx`
- Crate source (51.0.0): shows the correct jsx_preserve branch exists
- Version mapping attempts (librarian) contradicted themselves and were discarded

**Consequence/design:**
- Worker (Rust crate) uses native flag — works correctly
- Node-binding path (lage `swcWorker` / @swc/cli) MUST keep `rewriteImportExtensions` regex post-processor (`.ts/.tsx/.mts/.cts`→`.js`)
- This is VERSION-LAG/DISTRIBUTION DIVERGENCE, not permanent-by-design
- **Re-test on @swc/core upgrades** — future binaries may ship the fixed static rewriter
- **Do NOT trust librarian version-mapping** for this; trust only direct crate-source inspection + live npm probe

### externalHelpers + Yarn PnP Strict (Regression + Fix)

**Problem:**
SWC externalizes helpers by default → emits `import { _ } from "@swc/helpers/_/..."`. Under `nodeLinker: pnp` + `pnpMode: strict`, packages that don't declare `@swc/helpers` can't resolve it → webpack `Module not found`.

**Why it surfaced:**
Prior Babel path INLINED helpers. Switching Babel→SWC exposed latent missing-dependency.

**Fix:**
```json
// swc.browser.json / swc.node.json / programmatic config
{
  "jsc": {
    "externalHelpers": false
  }
}
```

Set in BOTH:
- Programmatic config (`makeSharedJscConfig` → Node-binding path)
- Generated `.swcrc` (`swcrcConfigFor` → worker path)

**Impact:** avoids adding `@swc/helpers` dependency to ~100 packages.

**General lesson:** transpiler swaps change helper externalization. Verify against strict PnP.

### CJS Named-Export Interop for ESM Consumers

`module.exportInteropAnnotation: true` makes SWC's CJS output cjs-module-lexer-readable:

```js
// SWC emits (in addition to live-binding helper loop):
0 && (module.exports = { Baz: null, bar: null, foo: null });
```

Node ESM `import { x } from 'cjs-pkg'` works. Eliminates babel fallback for CJS node builds.

**Config:**
```typescript
module: {
  type: "commonjs",
  exportInteropAnnotation: true  // enables above annotation
}
```

### should_skip Must Match is_transformable

Worker skip-filter extension list must equal transformable extension list (js/jsx/ts/tsx/mjs/cjs/mts/cts) or test/story files with newer extensions leak into dist.

**Prevention:** reuse `is_transformable()` for skip logic — prevents drift.

### sourceMappingURL Only When Map Is Produced

Append `//# sourceMappingURL=` only when source map is actually emitted:

```rust
if let Some(ref map) = output.map {
    emit_source_map(path, map);
    emit_code.push_str(&format!("\n//# sourceMappingURL={}.map\n", file_stem));
}
```

Gate on `output.map.is_some()` — else disabled-source-maps output references nonexistent `.map`.
