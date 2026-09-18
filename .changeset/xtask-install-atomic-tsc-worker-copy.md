---
luchta: patch
---
Fix `cargo xtask install` failing with "Text file busy (os error 26)" when
installing `luchta-tsc-worker` while a previous instance of it is still
running. The install now copies the built binary into a temporary file next
to the destination and renames it into place, matching how every other
install step already replaces a running binary, instead of copying directly
onto the destination path.
