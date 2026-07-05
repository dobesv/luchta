---
luchta: minor
---

Fix race condition where input file edits during task execution caused stale cache metadata (issue #157)

When a user edits an input file (H1→H2) while a task is running, the output is produced from H1 but the cache metadata was storing H2. This caused:
- Local cache incorrectly skipping reruns on real changes
- Shared cache restoring stale outputs
- Watch mode missing rebuilds

The fix captures a pre-execution input snapshot (content hashes) before the task runs, then compares it against post-run resolution. On mismatch, cache write is skipped and the task remains dirty for watch mode.

The strict concurrent-change guarantee applies to declared inputs. Worker-detected inputs (files a worker discovers only during the run) have no pre-execution baseline, so they are recorded best-effort with their post-run hash; a change to a detected input between runs is still caught by the normal cache decision on the next run. Declare inputs when strict correctness matters.
