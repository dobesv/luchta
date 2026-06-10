---
luchta: minor
---
Add stay-resident worker processes: tasks can opt into a named long-lived
worker (via the `workers` config map and a task's `worker` field) that accepts
jobs over a JSONL stdin/stdout protocol, eliminating per-task process startup
cost. Includes the `luchta-yarn-worker` reference worker binary. Resident
workers are supported on Unix only.
