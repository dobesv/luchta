---
luchta: major
---
# Run yarn scripts directly without a yarn process per task

This is a breaking change for two configurations: projects using the
`node-modules` linker (no `.pnp.cjs` is produced, so direct mode has nothing
to read) and tasks whose `command` is a yarn CLI command rather than a
`package.json` script (for example `install`, `dlx`, or `run build`, which
direct mode looks up as a script named `install`/`dlx`/`run` and won't find).
Both now fail the task until you pass `--no-direct` on the worker command.

`luchta-yarn-worker` now computes the environment Yarn would inject (bin
shims on `PATH`, `NODE_OPTIONS` with the PnP loader, `npm_*` variables) from
the project's `.pnp.cjs` and runs the script body under `bash`, so each task
no longer pays for a `yarn` process. Direct mode is the default and fails
tasks with a descriptive message when it cannot apply; pass `--no-direct` on
the worker command to restore the previous `yarn workspace <pkg> <command>`
behavior.
