---
luchta: minor
---
Add task `tools` declarations for entries in the workspace-root `.tool-versions`. Under `--since`, version changes select declaring tasks' packages and their transitive dependents, plus declaring root tasks. Missing entry names warn once without failing the run. Duplicate tool names use the first entry; later entries are ignored.
