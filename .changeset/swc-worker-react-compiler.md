---
luchta: minor
---

Add React Compiler support to the SWC transform worker. Enable it per project
in `.swcrc` with `{"jsc":{"transform":{"reactCompiler":true}}}`. The compiled
output imports `react/compiler-runtime`, so consuming packages need React 19+
(or the `react-compiler-runtime` package) available at runtime. Also bump
`swc_core` to `74.0.2`.
