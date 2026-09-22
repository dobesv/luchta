---
luchta: patch
---
Improve watch reliability on macOS by using one recursive FSEvents watch, surfacing backend failures, and recovering from dropped filesystem events with a full rescan.
