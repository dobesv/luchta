---
luchta: patch
---
Prevent worker stdin `SIGPIPE` errors from terminating Luchta before it can report task failures, while retaining quiet exits for broken stdout pipes.
