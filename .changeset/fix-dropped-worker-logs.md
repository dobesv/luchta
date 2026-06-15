---
luchta: patch
---
Fix dropped worker log lines under high output volume. The shared stdout reader now applies back-pressure (awaiting the bounded per-job channel) instead of dropping responses with a "worker response queue full" warning when a job produced logs faster than they could be printed.
