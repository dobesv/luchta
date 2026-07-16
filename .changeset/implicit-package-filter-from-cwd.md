---
luchta: minor
---

Apply an implicit `-p <package>` filter when `run`, `watch`, `logs`, or `why` is invoked from within a package subdirectory, scoping the command to that package as if `-p <name>` had been passed. Explicit `-p` and `--top-level` still win, running from the workspace root applies no implicit filter, and workers remain pinned to the workspace root. Workspace-root resolution now walks upward from the current directory to locate the monorepo root.
