---
luchta: patch
---
Batch shared-cache index updates into a single end-of-run merge, reducing lock contention and remote synchronization work for runs that store many tasks.
