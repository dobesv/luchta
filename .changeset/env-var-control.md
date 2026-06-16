---
luchta: minor
---
Add environment-variable control for tasks: declare env at global, worker, and task scopes (precedence task > worker > global); resolve with set/setDefault/inherit/inheritCacheIgnore semantics; run subprocesses in strict mode (cleared env + built-in passthrough whitelist + declared vars); integrate the effective env into the cache hash correctly; and detect same-variable set+setDefault conflicts in `luchta check`.
