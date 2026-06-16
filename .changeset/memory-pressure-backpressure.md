---
luchta: minor
---
Add memory-pressure backpressure: pause dispatching new tasks when process-tree RSS exceeds --mem-usage-threshold or system available memory drops below --mem-free-threshold (configurable via flags/env, percent or absolute units).
