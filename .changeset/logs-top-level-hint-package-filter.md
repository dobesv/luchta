---
luchta: patch
---
Stop advising `-T/--top-level` when a package filter (`-p`) targets a package that lacks the requested task but the task exists as a package task elsewhere. `luchta logs -p @repo/foo build` (where `build` exists at the root and in another package, but not `@repo/foo`) now reports the plain "not found in task graph" error instead of the misleading top-level hint.
