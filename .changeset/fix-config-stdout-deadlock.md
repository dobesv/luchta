---
luchta: patch
---
Fix config loader deadlock that misreported large configs as `config script timed out`. Stdout from the config script is now drained concurrently with the process wait, so scripts whose output exceeds the OS pipe buffer (~64KB) no longer block until the timeout.
