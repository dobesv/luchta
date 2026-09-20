---
luchta: patch
---
Include worker-reported tool versions in task cache keys so tool updates can
invalidate local and shared caches. Tasks without a reported version keep their
existing cache keys. Shared-cache snapshots record the reported version for
inspection without adding a separate cache-hit check.
