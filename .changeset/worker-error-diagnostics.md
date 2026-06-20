---
luchta: minor
---
Improve worker crash diagnostics: failures now report which worker and task(s) failed, the exit reason (`exited with code N` or `killed by signal SIGKILL (9)`), and the worker's stderr tail as a delimited block — instead of a bare broken-pipe message. `WorkerError::Protocol` now includes the worker name, and middleware (yarn-filter, lazy-worker, command-filter, file-exists-filter) exits non-zero to its own stderr on delegate failure rather than emitting `delegate failed before done` JSONL (closes #106, #65).
