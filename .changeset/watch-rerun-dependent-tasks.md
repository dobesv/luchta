---
luchta: patch
---
Watch mode now re-runs tasks that depend on a task after it is fixed or re-run (previously dependent tasks were skipped after a failed-then-fixed upstream task). Fixes #186.
