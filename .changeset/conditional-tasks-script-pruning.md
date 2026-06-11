---
luchta: minor
---
Global tasks now prune gracefully when a package lacks the matching script.

During graph construction each worker-backed task is resolved through its worker
(a new `ResolveTask` protocol message). `luchta-yarn-worker` keeps a task only
when its resolved script name — the explicit `command`, otherwise the task name —
exists in the target package's `package.json` `scripts`; otherwise the task is
pruned and never enters the execution graph. A worker may instead `Modify` a
task — replacing its command, dependencies, and/or weight. Tasks that depend on
a pruned task still run (the dropped edge is tolerated); a requested task that
was pruned everywhere is reported as such rather than as "not found".
`luchta run`/`--dry-run` report which tasks were pruned and why; `luchta check`
lists prunes informationally and reports a worker rejection as an error.

Root (`#task`) tasks now render as `#task` in output instead of leaking the
internal `//root` package id.
