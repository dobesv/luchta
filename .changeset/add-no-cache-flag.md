---
luchta: minor
---
Add `--no-cache` flag and `LUCHTA_NO_CACHE` env var to `run` and `watch` subcommands. When active, tasks always run (no skipping), shared cache is neither read nor written, but local workspace cache metadata is still updated so subsequent normal runs can skip tasks as usual. Closes #123.
