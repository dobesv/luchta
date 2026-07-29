---
luchta: major
---
# Path globs no longer let `*` cross directory separators, and support `!` negation

`inputs`, `outputs`, the `workspaces` globs in the root `package.json`, watch
patterns, and `luchta-file-exists-filter` now compile with globset's
`literal_separator` enabled. `*` and `?` match within a single directory level,
the way `.gitignore`, Turborepo, and lage all behave. Recursion is explicit:
`src/*.ts` no longer matches `src/deep/a.ts` — write `src/**/*.ts`.

**Review your `inputs` and `outputs` before upgrading.** A pattern like
`dist/*.js` that silently matched nested files now matches only the top level,
which narrows what feeds the cache hash. The cache schema is bumped to V5 so
records written under the old semantics are discarded instead of reused.

This also fixes workspace discovery: `"workspaces": ["packages/*"]` no longer
picks up nested `package.json` files such as fixtures or templates inside a real
workspace package. Yarn never treated those as workspaces.

A leading `!` now excludes:

```js
inputs: ["src/**", "!src/**/*.test.ts"],
outputs: ["dist/**", "!dist/**/*.map"],
```

Negations always win regardless of order, and in `inputs` one negation filters
files resolved from every base directory — your own package, `#` root patterns,
and each `^`/`^^` upstream package. Escape a literal leading bang as `\!`.

Package and task **name** globs (`-p`, task arguments, the `dependencies`
filter) are unchanged: `*` still crosses `/` there so `-p '*'` keeps matching
scoped names like `@scope/pkg`.
