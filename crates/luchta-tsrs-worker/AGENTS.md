# luchta-tsrs-worker

In-process TypeScript worker embedding oxc-project/tsrs (a Rust port of `tsgo`,
TypeScript 7.1.0-beta). Opt-in alternative to the Go `luchta-tsc-worker`.
Behaviour reference: `patches/luchta.patch`, `tsc/internal/luchta/`. Match
protocol structure (exit codes, output sets, report shape), not diagnostic text.

## tsrs program lifetime

tsrs allocates in bump arenas with no Drop and `new_program` returns
`&'static Program`. Every compile is one-shot. The pattern in `src/compile.rs`
is load-bearing:

- Compile and emit inside `spawn_blocking` + `catch_unwind`; the `OwnedProgram`
  guard calls `free_unshared_program` on every path (diagnostics, write
  failures, panics). Don't replace it with manual frees on the happy path.
- No tsrs pointer may escape the compile task: convert diagnostics to owned
  `SarifFinding`s before freeing, serialize SARIF afterward.
- Any new tsrs-allocating path — config parsing included
  (`src/inputs.rs::resolve_inputs`) — must scope allocations in a
  `tsrs_core::arena::Region`. Without it, arenas accumulate in reused blocking
  threads and RSS grows per task. The Linux RSS regression tests
  (`src/compile/tests.rs::compile_sequential_rss_is_bounded` and
  `tests/resolve.rs::resolve_sequential_rss_is_bounded`) exist to catch
  exactly this; keep them passing when touching lifetime code, and add one
  for new tsrs entry points.
- Known upstream gap: a panic inside `new_program` before it returns leaks
  partially constructed boxes — there is no pointer to free. See
  https://github.com/oxc-project/tsrs/issues/646 and the comment in
  `create_program`. Don't claim full cleanup for pre-return panics.

## Bumping tsrs

tsrs crates are unpublished git deps pinned to one upstream rev; `tsrs_core` is
redirected to the dobesv/tsrs fork (relaxed `unicode-id-start`, see the
`[patch."https://github.com/oxc-project/tsrs.git"]` comment in the root
`Cargo.toml`). On each bump: rebase the fork onto the new upstream rev, update
the `[patch]` rev to match, and keep all upstream deps on one rev. The `xtask`
test `tsrs_git_dependencies_share_one_revision` enforces both pin sets.

A bump changes `LUCHTA_TOOL_VERSION` — `build.rs` emits the `tsrs_compiler` and
`tsrs_core` revisions — which invalidates task caches for this worker. That's
intended. Renovate groups tsrs bumps, but neither Renovate nor Mergify merges
them automatically. Before merging, rebase the dobesv/tsrs fork onto the new
upstream rev and update the `[patch]` rev. The pin guard doesn't check that the
fork is based on the upstream rev.

## PnP

Parse tsconfig through `CompilerSystem` (the PnP-aware FS), never plain osvfs
plus a program resolver hook: bare `extends` resolves during parsing, before
`ProgramOptions.create_module_resolver` runs. Details in the `src/pnp.rs`
module docs.
