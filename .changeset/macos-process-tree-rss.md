---
luchta: patch
---
Report process-tree memory usage on macOS and Windows. Previously the `🐏`
RSS gauge always showed `0 B` off Linux because `process_tree_rss_bytes_for`
was a Linux-only `/proc` reader; non-Linux platforms now enumerate the process
table via `sysinfo`, so usage-based memory backpressure works there too.
