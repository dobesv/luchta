---
luchta: patch
---
Stop macOS memory readings from stalling task dispatch

macOS reports available memory in a way that makes the dispatch pressure gate
pause a healthy machine indefinitely. Three macOS-only fixes; Linux and Windows
behavior is unchanged:

- On macOS the pause loop never holds the last task back. With nothing in flight
  no build work is left to release memory, so waiting could only deadlock the
  run — which is what happened when pressure came from other applications or a
  bad reading.
- macOS availability is now read from the kernel's own VM counters as
  `free + inactive + purgeable`, without subtracting the compressor. Compressed
  pages are already counted as used, and the previous reading shrank precisely
  when a build worked hardest. An elevated `kern.memorystatus_vm_pressure_level`
  — the signal behind Activity Monitor's pressure graph — now pauses dispatch on
  its own.
- Per-process memory on macOS comes from `ri_phys_footprint` instead of resident
  size, which counted every shared page once per process and inflated the tree
  total across a fan-out of workers.
