---
luchta: patch
---
Fix cross-package inputs that share a relative filename making a task permanently uncacheable (#138). Input file paths are now recorded repo-relative so files from different packages no longer collide on the same cache key.
