---
luchta: patch
---
Speed up `--since` by skipping input checks for already-affected packages and reusing compiled matchers for tasks with identical inputs in the same package. Fixes #945.
