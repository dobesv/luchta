---
luchta: patch
---
Speed up cached (no-op) builds. Change detection now walks each package directory
only once per run instead of once per task, and reuses a file's prior content hash
when its size and mtime are unchanged instead of re-hashing it. On a large workspace
a fully-cached `luchta run build` drops from ~37s to ~19s (#154).
