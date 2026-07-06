---
luchta: minor
---
Parallelize local cache-skip decisions in the dispatcher so no-op ("all cached") builds no longer serially glob and stat every task's inputs on a single loop. The pure local cache-skip decision is now offloaded to a bounded blocking pool (semaphore sized to available parallelism), while shared-cache restores stay on the sequential dispatch loop to preserve their filesystem side effects and ordering. Also stores the package graph behind an `Arc` in the cache resolver and write context to avoid deep-cloning it per task. On a large monorepo this cuts a no-op build from ~9.6s to ~4s.
