---
luchta: patch
---
Flush shared-cache entries again after task shutdown so tasks finishing during cancellation are less likely to lose their cache index entries.
