---
luchta: patch
---
Fix the Go TypeScript worker's stale-output cleanup deleting declarations emitted by earlier tsconfigs in the same run and authored declaration inputs listed in a config's root files. Preserve their declaration maps too. Remove duplicate diagnostics when `noEmitOnError` blocks emission, and invalidate cached tsc results for the worker fixes.
