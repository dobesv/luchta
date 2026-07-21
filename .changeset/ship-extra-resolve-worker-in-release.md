---
luchta: patch
---
Include `luchta-extra-resolve-worker` in the release smoke-test probe list so tagged/dispatched release builds no longer fail the probe-drift guard. The binary was already installed by `cargo xtask install` and packaged into release archives (both are metadata-driven); this fixes the release workflow's smoke test that had drifted.
