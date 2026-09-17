---
luchta: patch
---

# Collapse memory-pressure sensitivity to on/off

`--mem-pressure <off|low|normal|high>` promised a four-level ladder that was
never real on two of the three platforms it covered. Windows exposes a single
low-memory bit, so sensitivity was always a documented no-op there. macOS
exposes only three levels (normal / warning / critical), and only critical is
worth pausing on — macOS raises *warning* routinely as an advisory to release
cached memory, not as a sign the machine is struggling. Pausing dispatch on it
stalled builds on an otherwise healthy Mac (`❗ memory pressure (warning)`).
Closes #347.

Linux PSI is the only genuinely continuous signal among the three, so it's the
only place a dial ever meant anything.

`--mem-pressure` is now `--no-mem-pressure` (`LUCHTA_NO_MEM_PRESSURE`), a
plain on/off switch mirroring `--no-cache`. In its place, each platform that
actually has something to tune gets its own env var:

- `LUCHTA_MEM_PSI_THRESHOLD` (Linux) — percent of `full avg10` above which
  dispatch pauses. Default `60`, unchanged from the `normal` sensitivity.
- `LUCHTA_MEM_MACOS_LEVEL` (macOS) — `warning` or `critical`. Default
  `critical`, tighter than the old `normal`/`high` default of `warning` — this
  is the behavior change that fixes #347.
- Windows has nothing to tune; the switch is still on/off there.

Setting either tuning variable to something unparseable is a startup error
naming the variable and what it accepts, not a silent fallback to the default.
