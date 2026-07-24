---
title: "Enable SWC native React Compiler transform in luchta-swc-transform-worker"
date: 2026-07-23
category: integration-issues
problem_type: integration_issue
component: luchta-swc-transform-worker
root_cause: "compile-time feature gate required for swc react-compiler transform; activation requires both Cargo feature and runtime .swcrc config"
resolution_type: code_fix
severity: medium
tags:
  - swc
  - react-compiler
  - feature-gating
  - swc_core
  - transform-pipeline
plan_ref: swc-react-compiler
---

## Problem

Enable React Compiler support in the SWC transform worker. React Compiler memoizes components by rewriting them to use a runtime (`react/compiler-runtime`) that allocates memoization caches. SWC >= 73 includes a native Rust port of Meta's React Compiler, but it requires a compile-time Cargo feature to activate.

## Symptoms

Without the `react-compiler` Cargo feature, `Compiler::process_js_file` hard-errors when `.swcrc` configures `reactCompiler`:

```
swc was built without the `react-compiler` feature
```

No transform occurs. No memoization cache allocated. Output is plain JSX→JS.

## Investigation Steps

1. Read [swc-project/swc PR #11917](https://github.com/swc-project/swc/pull/11917) which landed React Compiler into SWC core.
2. Verified `swc_core` >= 73 exposes `base_react_compiler` feature (tested on 74.0.2).
3. Checked `swc_ecma_react_compiler` crate — vendors Meta's compiler via `forked_react_compiler_*` crates (Boshen/oxc-project publishes these since Meta doesn't publish to crates.io). Source is verbatim upstream; only Cargo.toml patched.
4. Confirmed `TransformConfig.react_compiler: BoolOrDataConfig<ReactCompilerConfig>` already deserializes `.swcrc` — no worker-side config changes needed.
5. Used probe test with `panic!("output: {}", out.code)` to discover exact emitted output before writing assertions.
6. Identified two-part activation: (1) compile-time feature, (2) runtime config.

## Root Cause

SWC's React Compiler transform is feature-gated at two levels:

1. **Compile-time**: The `swc` base crate requires the `react-compiler` Cargo feature. From `swc_core`, enable via `base_react_compiler` feature, which chains to `swc/react-compiler`. Without this, the transform code isn't compiled in.

2. **Runtime**: Users opt in per-project via `.swcrc`:
   ```json
   { "jsc": { "transform": { "reactCompiler": true } } }
   ```
   Or with options:
   ```json
   { "jsc": { "transform": { "reactCompiler": { "compilationMode": "infer", "target": "react19" } } } }
   ```

Both must be satisfied for the transform to run.

## Solution

Added one feature flag to worker's `Cargo.toml`:

**crates/luchta-swc-transform-worker/Cargo.toml:**
```toml
[dependencies]
swc_core = { workspace = true, optional = true, features = ["base_react_compiler"] }
```

This chains through `swc_core`'s feature:
```
base_react_compiler = ["__base", "swc/react-compiler"]
```

No other code changes. Worker already calls `Compiler::process_js_file`, which is the exact entry point SWC uses for React Compiler when the feature is enabled.

**Workspace Cargo.toml bump:**
```toml
# swc pinned exact — SWC breaks public API on minor bumps
swc_core = { version = "=74.0.2", features = ["base_module", "common", "ecma_ast", "ecma_parser", "ecma_helpers_inline"] }
```

Bumped from `=73.0.0` → `=74.0.2`. Worker's usage of `Compiler`/`Options`/`process_js_file` unaffected. Full workspace suite (1359 tests) green.

**Test coverage (transform.rs):**

```rust
const REACT_COMPONENT_SOURCE: &str =
    "export function Foo({ items }: { items: number[] }) {\n  return <ul>{items.map((n) => <li key={n}>{n}</li>)}</ul>;\n}\n";

#[test]
fn react_compiler_memoizes_components_when_enabled() {
    let temp = assert_fs::TempDir::new().expect("temp dir");
    temp.child(".swcrc")
        .write_str(r#"{"jsc":{"parser":{"syntax":"typescript","tsx":true},"transform":{"reactCompiler":true}}}"#)
        .expect("write .swcrc");
    let source = temp.child("src/index.tsx");
    source.write_str(REACT_COMPONENT_SOURCE).expect("write source");

    let args = SwcArgs::parse("", Some(temp.path())).expect("parse args");
    let out = transform_source(&args, source.path(), REACT_COMPONENT_SOURCE, Path::new("src/index.tsx"), "index.js.map")
        .expect("transform succeeds");

    assert!(out.code.contains("react/compiler-runtime"),
        "React Compiler output should import the compiler runtime");
    assert!(out.code.contains("_c("),
        "React Compiler output should allocate memoization cache via _c()");
}

#[test]
fn react_compiler_not_applied_when_disabled() {
    // Same test without reactCompiler in .swcrc
    // Asserts !out.code.contains("react/compiler-runtime") and !out.code.contains("_c(")
}
```

## Why This Works

`Compiler::process_js_file` is SWC's unified transform entry point. When compiled with `react-compiler` feature and `reactCompiler` config present, it executes the React Compiler pass as part of the transform pipeline. No worker code needed beyond enabling the feature gate.

React Compiler output signature:
```js
import { c as _c } from "react/compiler-runtime";
// ...
var $ = _c(4);  // memo cache allocation
```

Tests assert on `"react/compiler-runtime"` import and `"_c("` call — reliable markers of successful compilation.

## Prevention Strategies

**Cargo feature checklist:**
- When adding SWC transforms, check `swc` and `swc_core` Cargo features first
- `swc_core` re-exports features with `base_` prefix: `base`, `base_module`, `base_react_compiler`
- Runtime config (`jsc.transform.*`) alone is insufficient without compile-time feature

**Testing approach:**
- Use probe tests (`panic!("{}", out.code)`) to discover actual output before writing assertions
- Assert on stable output markers (imports, function calls), not exact code
- Test both enabled and disabled paths

**Version pinning:**
- SWC breaks public API on minor bumps — pin exact version (`=X.Y.Z`)
- Re-run full workspace tests after any SWC version change

**Runtime requirements:**
- Projects using React Compiler need React 19+ or `react-compiler-runtime` package at runtime
- Changeset should document runtime dependency

## Related Issues

- **GitHub Issue:** [#264](https://github.com/dobesv/luchta/issues/264) — Add React Compiler support
- **SWC PR:** [swc-project/swc#11917](https://github.com/swc-project/swc/pull/11917) — React Compiler integration into SWC core
- **Related Solution:** [swc-worker-in-process-integration-2026-07-08.md](./swc-worker-in-process-integration-2026-07-08.md) — Initial SWC worker setup, feature gating patterns
