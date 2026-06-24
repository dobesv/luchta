---
luchta: minor
---

Introduce a new `--continue` flag for `luchta run` to keep building independent tasks after a failure, skipping only the failed task's transitive dependents. By default, `luchta` now employs an aggressive fast-stop strategy on the first failure: it stops dispatching new tasks and actively terminates in-flight workers (sending SIGTERM, followed by SIGKILL after a 1-second grace period).

The UI has been updated to show failed tasks in the status line and final summary as `× N (names)`. The redundant "one or more tasks failed" error message was removed, and the final execution summary now prints on failure to provide clear statistics. The run still exits with a non-zero code if any task fails. Fixes #101 and #82.
