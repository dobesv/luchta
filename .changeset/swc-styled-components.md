---
luchta: minor
---
# Run `@swc/plugin-styled-components` natively in the SWC transform worker

The SWC transform worker (`luchta-swc-transform-worker`) now runs `@swc/plugin-styled-components` natively. Enable it per project in `.swcrc` via `jsc.experimental.plugins` (e.g., `[["@swc/plugin-styled-components", { "displayName": true, "ssr": true }]]`). Unrecognized SWC plugins now fail the build with an explicit error naming the unsupported plugin instead of being silently skipped.
