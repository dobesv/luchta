---
luchta: minor
---
Add `-p`/`--package` flag to `luchta run` for targeting specific packages by name (not path). Both the package flag and task arguments now support glob wildcards. Features a "goal-not-filter" selection model: filters pick entry-point goals, while Luchta ensures all transitive prerequisites are executed regardless of the filter.
