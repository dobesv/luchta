---
luchta: minor
---
Add in-process oxc transform (babel replacement) and oxfmt (formatter) workers: luchta-oxc-transform-worker transpiles src/** to dist/<env> and reports outputs; luchta-oxfmt-worker formats in place or checks with OXFMT_OPTS=--check. Unix-only.
