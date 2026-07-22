---
luchta: minor
---
Add `cache.sharing` task config field (`"none" | "local" | "remote"`, default `"remote"`) to control whether a task's cache entries may use the shared/remote cache tier, independent of local caching. Closes #103.
