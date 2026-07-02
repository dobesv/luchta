---
luchta: minor
---
Luchta watch mode now detects structural workspace changes—such as adding, removing, renaming, or moving packages—without requiring a restart. The workspace is re-discovered and graphs are rebuilt live when the set of packages changes, while keeping the worker pool active for seamless execution. Ordinary source-file edits are unaffected and do not incur discovery overhead.
