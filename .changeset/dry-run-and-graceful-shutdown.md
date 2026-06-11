---
luchta: minor
---
Add a `--dry-run` flag to `luchta run` that prints the selected tasks grouped
into parallel "waves" (each task only depends on tasks in earlier waves),
annotated with the worker/command each would run, without executing anything.
This gives a clear view into the computed task dependency graph for debugging
ordering and configuration issues.

Make interruption and shutdown graceful: Ctrl-C (SIGINT) and SIGTERM now stop a
run promptly, terminate worker process groups with SIGTERM before escalating to
SIGKILL (so child build tools exit cleanly instead of dumping stack traces),
and reap all workers so nothing is orphaned. Interrupted runs no longer flood
the terminal with per-task crash output, and piping `luchta run` into a command
that closes early (e.g. `| head`) no longer panics with a broken-pipe error.

Loading an already-executable config script no longer fails on a read-only
filesystem (the executable bit is only set when needed).
