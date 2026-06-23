---
luchta: minor
---

`luchta logs --show-inputs` and `--show-outputs` now also display the effective
input/output patterns (globs) stored in the task's cache metadata, each marked
`(detected)` or `(declared)` depending on whether they came from worker
detection or declared task config. This is shown in addition to the list of
files matched at build time, making it easier to debug dependency-updating
issues. Fixes #120.
