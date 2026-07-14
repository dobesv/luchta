---
luchta: minor
---
Workers can now contribute a runtime cache nonce that participates in each task's cache key. A new `workerNonce` scope is threaded through the resolve protocol (`ResolveResult.cache_nonce`) and folded into the task spec hash, so upgrading a worker binary or changing its runtime identity automatically invalidates cached results produced by that worker. The eight in-tree workers report their crate version as their nonce; workers that supply no nonce behave exactly as before (no cache change). Closes #227.
