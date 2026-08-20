---
luchta: minor
---
# Fix macOS worker shutdown and trim the release platform matrix

Treat `EPERM` from `kill(-pgid)` as "process group already gone" on Apple
targets. Darwin returns `EPERM` rather than the POSIX `ESRCH` when the group
still exists but has no live members, so shutting down a delegate that had
already exited failed with `Operation not permitted`.

Stop publishing `x86_64-apple-darwin` and `i686-pc-windows-msvc` binaries.
Intel Macs and 32-bit Windows must build from source.
