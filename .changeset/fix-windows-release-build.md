---
luchta: patch
---
Fix the Windows release build. Two Unix-only code paths were compiled unconditionally: the worker proxy's `terminate_child`/`kill_child` (now have `#[cfg(windows)]` implementations via `Child::start_kill`), and the `hyperlocal`-based S3/rclone shared-remote-cache layer (now gated behind `#[cfg(unix)]`, so the shared cache runs local-only on Windows — matching its existing degrade-to-local behavior). PR CI now builds and lints on Windows and macOS in addition to Linux, so platform-specific errors surface on pull requests instead of at release time.
