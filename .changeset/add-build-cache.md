---
luchta: minor
---
Add opt-in filesystem-backed build change-detection cache (`cache: {}`) that skips successful tasks when declared or worker-detected inputs, outputs, significant env, lockfile-resolved package dependencies, and dependency-task outputs stay unchanged, while persisting cache logs for failed-task reporting.
