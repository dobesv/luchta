---
luchta: patch
---

Fix `aarch64-pc-windows-msvc` and `i686-pc-windows-msvc` release builds by dropping the unused SWC wasmer plugin backend. It pulled in `wasmer` → `corosensei`, which does not support those targets and failed the release with `compile_error!("Unsupported target")`.
