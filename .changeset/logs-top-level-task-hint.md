---
luchta: patch
---
Replace the misleading "task not found in task graph" error with an actionable hint when a requested task exists only in the other scope. Asking for a top-level `#task` without `-T` (e.g. `luchta logs audit-licenses`) now suggests passing `-T/--top-level`, and asking for a package task with `-T` suggests dropping it. Applies to `run`, `logs`, `list`, and `why`.
