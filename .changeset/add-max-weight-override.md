---
luchta: minor
---
Add `--max-weight <WEIGHT>` CLI flag and `LUCHTA_MAX_WEIGHT` environment variable to override `concurrency.maxWeight`. Precedence: CLI flag > env var > config file > default. Empty or zero override values are rejected.
