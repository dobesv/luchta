---
luchta: minor
---
Add `luchta-worker-watcher`, a worker wrapper that watches file globs and auto-restarts the delegate worker on change, draining in-flight operations before shutting the old instance down (#170).
