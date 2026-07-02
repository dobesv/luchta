---
luchta: patch
---
Fix watch mode rebuilding the same packages endlessly (#161) and no longer blank
the screen on each rebuild (#160).

Watch mode now only rebuilds a package when a changed file is an actual **input**
to one of its tasks. As each task runs, luchta records that task's resolved input
files (with size, mtime, and content hash) plus its input glob patterns. A
filesystem event triggers a rebuild only when it is a real content change to a
known input, or a new file matching a task's input globs. Restored cache outputs,
build artifacts, and touch-only events are ignored — which stops the self-perpetuating
rebuild loop.

Watch mode no longer clears the terminal on each rebuild, preserving scrollback so
previous build output and change history stay visible.

New `luchta watch --show-changed-files` flag lists the files that triggered each
rebuild (first 10 plus a count), useful for diagnosing unexpected rebuilds.
