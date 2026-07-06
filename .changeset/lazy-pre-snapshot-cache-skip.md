---
luchta: patch
---
Speed up fully-cached builds by skipping pre-execution input snapshot hashing on cache-hit tasks. Luchta now captures that snapshot only after a task is committed to run, while still taking it before command dispatch so concurrent input edits during execution remain detectable.
