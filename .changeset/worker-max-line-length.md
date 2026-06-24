---
luchta: patch
---

Worker stdout lines larger than 1 MiB no longer silently crash the job. The
per-line read cap (`MAX_LINE_LENGTH`) is raised from 1 MiB to 64 MiB so large
`report` payloads (e.g. multi-MiB SARIF) pass through, and an over-length line
now records a clear diagnostic ("worker output line exceeded MAX_LINE_LENGTH")
into the worker crash report instead of being discarded and surfacing only as an
unrelated-looking "delegate closed / Broken pipe" error. Generic I/O read
failures are reported as such rather than being mislabeled as length overflows.
Fixes #127.
