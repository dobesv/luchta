---
luchta: minor
---
Add `luchta-extra-resolve-worker` binary: a middleware worker that wraps a
resolve worker (consulted only during the resolve phase) and a delegate worker
(used for the run phase, and as the fallback for resolve when the resolve worker
returns Accept/Modify). This lets tasks be resolved at graph-build time even
when the run delegate is not ready, closing the gap where `luchta-lazy-worker`
must always Accept during resolution.
