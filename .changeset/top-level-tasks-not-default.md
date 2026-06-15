---
luchta: minor
---
`luchta run <task>` no longer matches or runs the top-level (workspace-root)
task of the same name. Un-prefixed task specs in the config now apply only to
non-root packages. Use `-T`/`--top-level` to run top-level tasks
(e.g. `luchta run -T build`); define top-level tasks with a `#` prefix
(e.g. `#build`). Fixes #69.
