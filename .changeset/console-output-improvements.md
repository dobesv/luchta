---
luchta: minor
---
Improve `luchta run` console output. The periodic status line and final summary line now use a compact emoji format (no legend): `☑️ done/total ⏭️ skipped ⌛ pending 🏃n (tasks) ⏱️ Ns 🐏 RSS 🌊 waves_done / total_waves`, with `⌛`/`🏃` segments omitted when zero. Skipped (cache-hit) tasks now fold into the done numerator, so `done` equals `total` at completion. Running tasks in the status line are grouped by task name then package (e.g. `{a,b,c}:lint, d:{test,tsc}, e#babel`); a shared npm scope is factored out of grouped package names (e.g. `@acme/{web,api}:lint`). Task failure messages drop the `Some(...)` wrapper, printing `failed with status 1` (or `failed` when terminated by signal).
