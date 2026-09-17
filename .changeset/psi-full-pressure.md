---
luchta: patch
---

# Fix spurious memory-pressure pauses on Linux

`luchta run` could pause with `❗ memory pressure (stalled …%)` on a machine
that had plenty of usable memory, and would often resume straight back into
the same reading and stall again.

The Linux backend was reading PSI's `some avg10`, which counts time when *any*
task stalled on memory reclaim — including the ordinary page-cache refaults
and direct reclaim an I/O-heavy build produces just by touching many files.
That kept `some` continuously nonzero, well above the old 10% trigger, with no
real memory shortage behind it.

Two changes fix it. Luchta now reads `full avg10`, the share of time *every*
non-idle task was stalled on reclaim at once — what the kernel's own PSI
documentation calls thrashing. And the trigger percentages move from 20% /
10% / 5% (`low` / `normal` / `high`) to 90% / 60% / 30%: field measurements
showed `full avg10` reaching 6% during a healthy, I/O-heavy build with no
pause, and around 11% at the point a spurious pause previously fired, so the
old single-digit thresholds sat inside normal build noise even after the
metric switch.

The new thresholds are also deliberately high-set: this gate is a last resort
against thrashing, not an early-warning memory governor. A paused build stalls
immediately on every run that meets the trigger, while running low on memory
is occasional and usually recoverable, so luchta would rather keep dispatching
well past the point a machine feels slow than pause early. Under `normal`,
expect luchta to keep going through memory conditions that look alarming;
`low` pauses only when the machine is nearly out of options.
