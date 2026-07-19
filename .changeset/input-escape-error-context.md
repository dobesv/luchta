---
luchta: patch
---
# Clearer input path-escape / expansion errors

Input path-escape and expansion errors now name the offending task and state
whether the problematic input pattern was declared in the task spec or returned
by the worker. The internal `//root` package sentinel is no longer leaked in
these messages — root tasks render as `#task` and the root package as
"the workspace root".
