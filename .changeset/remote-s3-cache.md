---
luchta: minor
---
Add an opt-in S3 remote build cache layer (LUCHTA_SHARED_CACHE=rclone:<spec>) backed by an rclone rcd sidecar, with append-only content-addressed snapshot shards and silent degrade-to-local on any remote error.
