---
luchta: patch
---
Further speed up change detection: the git worktree root is now discovered once
per run and reused for every package, instead of re-running `gix::discover`
(which walks up the filesystem to find `.git`) for every task/directory. On a
large workspace this cuts the internal change-detection time of a fully-cached
`luchta run build` by roughly 40% (#154).
