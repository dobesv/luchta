---
luchta: minor
---

# Update oxc to the `apps_v1.83.0` release

The bundled oxc toolchain moves to the commit tagged `apps_v1.83.0` /
`oxlint_v1.83.0` / `oxfmt_v0.68.0`, about two months' worth of upstream work.

Two things to know before you upgrade:

**The workspace now needs Rust 1.96.0**, the floor oxc itself declares at this
revision. Building Luchta from source on an older toolchain will fail with an
MSRV error; `rustup update stable` is enough to clear it. (See the third-party
dependency refresh changeset for where the workspace MSRV came from before
this bump.)

**Expect a one-time reformat the first time you run the oxfmt task.** oxfmt's
own JS/TS printer changed across this range — mostly where comments, semicolons
and added parentheses land — so a repo that was clean under the previous release
may report differences until those files are rewritten. Nothing in Luchta's own
formatting configuration changed; this is upstream's output moving underneath it.
A repo running `oxfmt --check` in CI will want to land the reformat in the same
change as the upgrade.

Embedded CSS-in-JS formatting is unaffected: `styled`/`css` template output is
byte-for-byte what it was before. oxlint diagnostics are unchanged too — same
rules, same findings, same messages.
