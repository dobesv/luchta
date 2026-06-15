---
luchta: minor
---
Add native worker-level `dependsOn` configuration (supporting both bare-string and object formats) and four new composable wrapper/filter workers: `luchta-lazy-worker`, `luchta-file-exists-filter`, `luchta-yarn-filter`, and `luchta-command-filter`. These tools allow for deferred worker startup and conditional task pruning during resolution, address #48 and #38.
