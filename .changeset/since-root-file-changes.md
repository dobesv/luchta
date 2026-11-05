---
"luchta": minor
---

# `--since` schedules tasks affected by root-level file changes

`luchta run --since <ref>` and `luchta list --since <ref>` now match changed files that live outside every workspace package against each task's declared `inputs`:

- A package task declaring a repo-root input (e.g. `#tsconfig.base.json`) is selected when only that root file changes, along with its transitive dependents.
- A top-level (`#task`) task that declares `inputs` is now filtered by `--since` like package tasks: it runs only when a changed file matches its inputs (e.g. `scripts/**`). Top-level tasks without declared inputs — or whose inputs expand to no positive include (negation-only, or `^`/`^^` at a workspace root with no upstream packages) — keep the existing behavior and still run under `-T`.
- Undeclared root edits (e.g. `README.md`) remain a clean no-op, and `!` negated input patterns are honored.

Fixes #943.
