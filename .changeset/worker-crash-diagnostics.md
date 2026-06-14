---
luchta: patch
---
Fix resident worker crash handling so dead worker processes are evicted and respawned instead of leaving later tasks stuck running forever, and include worker exit status plus captured stderr in crash errors.
