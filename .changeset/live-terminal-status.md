---
luchta: minor
---
# Show live progress on interactive terminals

Refresh progress in place ten times per second when stderr is an interactive
terminal with terminal control support (`TERM` is not `dumb`), while retaining
append-only five-second updates for redirected logs and dumb terminals. Long
running-task lists adapt to the terminal width, and normal output clears the
live status first so messages remain readable.
