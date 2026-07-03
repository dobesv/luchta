# luchta-worker-watcher

`luchta-worker-watcher` is a binary middleware for the luchta JSONL-over-stdin/stdout worker protocol. It wraps a delegate worker command, watches file globs for changes, and gracefully hot-swaps the delegate when a change is detected.

## Usage

```bash
luchta-worker-watcher --watch <glob>... [--debounce-ms <n>] -- <delegate cmd> [args...]
```

## Flags

| Flag | Description |
|------|-------------|
| `--watch <glob>` | **(Required)** One or more file globs to watch. This flag may be repeated. |
| `--debounce-ms <n>` | Debounce window in milliseconds for coalescing rapid file events. Defaults to `300`. |
| `--` | Separator between watcher flags and the delegate command. Everything after this is executed as the delegate worker. |

## How it works

The watcher implements a "hot-swap and drain" model to ensure zero downtime and safe transitions between worker versions:

1. **Detection**: When a watched file changes (after the debounce period), a new lifecycle event is triggered.
2. **New Generation**: The watcher spawns a **new generation** of the delegate worker process.
3. **Routing**: All **new inbound work** is immediately routed to the most recent generation.
4. **Draining**: Prior generations are allowed to finish their in-flight operations. They remain active until they are idle.
5. **Shutdown Ladder**: Once a generation is idle, it is shut down using a progressive ladder:
   - Close `stdin` (EOF)
   - Wait for process exit
   - Send `SIGTERM`
   - Wait for process exit
   - Send `SIGKILL`

Multiple draining generations can coexist and drain independently; the watcher never forces a kill on a generation that is still actively processing work.

## Limitations / Contracts

### 1. Resolve/Run Affinity is Unsupported
The watcher routes all new inbound work to the current generation and does **not** pin tasks to specific processes. If a delegate worker requires that a task's `resolve` and `run` phases happen on the same process, it is explicitly unsupported.

This watcher is intended to run behind `luchta-lazy-worker`, which handles resolution and passes `Run` messages to its delegate.

### 2. Globs do NOT respect `.gitignore`
Watched globs are matched directly against the filesystem. `.gitignore` files are intentionally ignored to support watching build outputs, caches, or other paths typically excluded from version control.

---
Part of the Luchta project (#170).
