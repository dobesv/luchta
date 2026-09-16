---
luchta: minor
---

# Memory-pressure backpressure now comes from the OS

Luchta asks the operating system whether memory is under pressure — Linux PSI
(the cgroup's `memory.pressure` where available, so container limits count),
macOS `kern.memorystatus_vm_pressure_level`, and the Windows low-memory
resource notification — instead of computing process-tree RSS and system
available memory against thresholds of its own.

**Breaking:** `--mem-usage-threshold` and `--mem-free-threshold`, and the
`LUCHTA_MEM_USAGE_THRESHOLD` and `LUCHTA_MEM_FREE_THRESHOLD` environment
variables, are removed. Use `--mem-pressure <off|low|normal|high>` or
`LUCHTA_MEM_PRESSURE`; the default is `normal`.

Two consequences worth knowing. Luchta no longer caps its own memory use as a
matter of course: it dispatches freely until the OS reports real pressure,
which is later than the old 50%-of-RAM ceiling fired. And where the indicator
cannot be read — a kernel built without `CONFIG_PSI`, or one needing `psi=1` —
backpressure is inactive and luchta says nothing about it.
