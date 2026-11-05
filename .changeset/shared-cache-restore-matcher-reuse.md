---
"luchta": patch
---

# Faster shared cache-file restore validation

Validating a shared cache-file restore now compiles the task's input and output glob matchers once, instead of once per restored file. Restores with many files or patterns no longer pay a glob compile per path. Fixes #946.
