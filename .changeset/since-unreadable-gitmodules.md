---
luchta: patch
---
Fix `--since` failing when `.gitmodules` is unreadable. Skip submodule configuration when the index has no submodules. If submodules exist but their configuration can't be read, warn and continue with submodules unconfigured.
