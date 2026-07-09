---
luchta: patch
---
Fix the release workflow failing on Windows targets: the `vendor/tsgo`
submodule was checked out recursively, pulling its nested
`_submodules/TypeScript` submodule whose long test-baseline paths exceed the
Windows path limit ("Filename too long"). The worker build never needs that
nested submodule, so checkout is now non-recursive (only `vendor/tsgo`), and
`core.longpaths` is enabled on Windows as a safety net.
