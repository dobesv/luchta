---
luchta: minor
---
Add `luchta-ast-grep-worker`: an in-process resident worker that lints codebases
using ast-grep custom YAML rules. Discovers `sgconfig.yml`, loads rule files,
and scans source files in-process via the ast-grep Rust library. Emits findings
as a SARIF report viewable with `luchta logs --file ast-grep.sarif`.
