---
luchta: patch
---
`luchta-yarn-filter` now checks for the task's own script name when deciding
whether to keep a task, ignoring any `command` override. The `command` is meant
for the underlying worker to run, so it no longer influences the filter's
default script-presence check (matching the intent that only the script name
gates filtering).
