---
luchta: minor
---

Add a user-controllable `cacheNonce` to force-bust stale cache entries. Set
`cache.nonce` at the global, worker, or task scope, or set the
`LUCHTA_CACHE_NONCE` environment variable. All four sources combine, so changing
any one invalidates the affected task's cache — useful for recovering from
poisoned cache entries that survive a worker bug fix. The resolved nonce is
persisted per task and inspectable via `luchta logs --show-cache-nonce`. Fixes
#118.
