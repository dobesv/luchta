---
luchta: minor
---
Speed up the oxlint, oxfmt, swc-transform, and oxc-transform workers by
processing files in parallel instead of one at a time. The oxfmt and transform
workers split their file set across worker threads (sized by available
parallelism), matching the ast-grep worker.

The oxlint worker now runs both regular and type-aware (tsgolint) linting over
the whole package in a single batched pass via oxc's `LintRunner`, instead of
invoking tsgolint once per file. Previously each file spawned its own tsgolint
process and reloaded the TypeScript program, making type-aware linting roughly
an order of magnitude slower than the `oxlint` CLI; batching per package makes
it dramatically faster (a ~36x speedup on a 60-file type-aware fixture) while
keeping suppression handling, `eslint-disable` support, and output unchanged.
