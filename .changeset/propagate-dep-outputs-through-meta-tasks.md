---
luchta: patch
---
Fix cached tasks not being invalidated when their `dependsOn` chain reaches the
changed outputs only through a command-less "meta" task. A task with no worker
and no command now contributes a hash derived from its own dependencies, so an
upstream output change propagates through any depth of such tasks.

Tasks that depend on a meta task re-run once after upgrading, because their
cache key now covers dependencies it previously ignored.
