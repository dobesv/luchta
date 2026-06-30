---
luchta: minor
---
Add `luchta watch` to automatically re-run affected tasks when files change. Uses package-level change detection with cache-based fine-grained skipping, keeps resident workers warm across rebuilds, and cancels in-flight builds when new changes arrive — no changes are lost.
