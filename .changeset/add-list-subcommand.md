---
luchta: minor
---
Add `luchta list` subcommand to list runnable tasks with their metadata.
Supports the same `-p`, `--top-level`, and task glob filters as `run`/`why`/`logs`.
Adds optional `--json` flag for machine-readable output.
Adds `description` field to task definitions.
