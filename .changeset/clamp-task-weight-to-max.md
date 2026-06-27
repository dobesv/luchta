---
luchta: minor
---
Tasks whose weight exceeds the executor's max weight are now clamped to the max weight and run, instead of failing with an error. A `concurrency.maxWeight` of 0 in config is now rejected at load time (matching the existing `--max-weight 0` CLI validation).
