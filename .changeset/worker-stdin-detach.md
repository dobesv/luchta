---
luchta: patch
---
Fix resident workers crashing mid-build with `Resource temporarily unavailable (os error 11)`. Job children inherited the worker's protocol stdin (fd 0); a process in the job tree (e.g. Node/libuv) could flip that shared open file description to `O_NONBLOCK`, causing the worker's next JSONL protocol read to fail with EAGAIN and kill the resident worker. Job children are now spawned with stdin detached (`/dev/null`). The yarn and bash workers also run on a single-threaded Tokio runtime now, since they only orchestrate async I/O.
