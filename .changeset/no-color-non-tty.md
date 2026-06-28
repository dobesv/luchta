---
luchta: patch
---
Stop emitting ANSI color/control codes to non-TTY output in `luchta run` dry-run and error paths by making CLI color formatting respect stream support. Fixes #134 and #43.
