---
luchta: minor
---
Add an in-process oxc-based oxlint worker (luchta-oxlint-worker) that lints via oxc_linter, emits SARIF reports, supports autofix and CLI-compatible bulk suppressions, and discovers .oxlintrc config. Unix-only; type-aware linting not yet supported.
