---
luchta: minor
---
Add `--output` flag to `luchta run` controlling progress output. Default mode emits wave-bucketed progress to STDERR every 5s (only when run exceeds 5s) plus a final summary line to STDOUT. New `--output summary` mode prints ONLY the final `Done: N tasks done after T seconds.` summary line to STDOUT. Skip accounting now reports cache-hit skips only. Interrupt diagnostics show running task count and RSS. Task failures dump captured output without a Done line. Default concurrency now uses the host's available parallelism instead of 1 (serial execution). The periodic progress interval (default 5s) can be overridden via the `LUCHTA_PROGRESS_INTERVAL_MS` environment variable.
