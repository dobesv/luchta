---
luchta: minor
---

Fix race condition where input file edits during task execution caused stale cache metadata (issue #157)

When a user edits an input file (H1→H2) while a task is running, the output is produced from H1 but the cache metadata was storing H2. This caused:
- Local cache incorrectly skipping reruns on real changes
- Shared cache restoring stale outputs
- Watch mode missing rebuilds

The fix captures a pre-execution input snapshot (content hashes) before the task runs, then compares it against post-run resolution. On mismatch, cache write is skipped and the task remains dirty for watch mode.

Workers now report their canonical file inputs at **resolve** time via `TaskModification.inputs` (fully replacing the task's declared inputs), and `Done` no longer carries inputs. Because worker-provided inputs are applied to the task definition before the pre-execution snapshot is taken, every tracked input — declared or worker-provided — has a pre-execution baseline and is covered by the same strict concurrent-change guarantee. There is no post-run best-effort input hashing: any input that requires cache correctness must be surfaced during the resolve phase.
