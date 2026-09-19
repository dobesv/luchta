---
luchta: patch
---
# Print a memory-pressure pause and resume notice under `--output summary`

A run held by the memory-pressure gate printed nothing under `--output
summary`, so it looked like a hang with no way to tell why. Dispatch now
emits a one-shot notice when it pauses on memory pressure (naming the reason
and the `--no-mem-pressure` escape hatch) and another when pressure clears.
The notices print in every output mode; periodic status lines under
`--output summary` stay suppressed.
