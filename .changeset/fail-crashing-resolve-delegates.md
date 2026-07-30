---
luchta: patch
---

# Fail when resolve delegates crash

Resolve and filter workers now fail when a resolve delegate crashes or times out instead of pruning the task and hiding the error. Command and file-exists filters also fail on predicate evaluation errors instead of treating them as no match.
