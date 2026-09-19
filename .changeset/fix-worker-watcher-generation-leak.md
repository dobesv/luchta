---
luchta: patch
---
Fix a worker-watcher process leak that could exhaust memory during a build. Each
watched-file change spawned a new delegate worker and left the previous one
"draining" forever — an idle draining worker was never reaped — so a run with
many file events accumulated one stuck worker per event. The watcher now runs at
most one worker at a time: on a restart it finishes the old worker's in-flight
tasks (queueing new work meanwhile), terminates it, then starts the replacement
and replays the queue. In-flight tasks are never abandoned, so a restart cannot
spuriously fail a build. Restarts are rate-limited and a wedged drain or shutdown
is bounded by a timeout. Set `LUCHTA_WORKER_WATCHER_DEBUG=1` to log worker
lifecycle transitions.
