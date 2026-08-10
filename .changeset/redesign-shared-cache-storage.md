---
luchta: minor
---
# Make shared caches portable across branches and machines

Store metadata by resolved input key and use bounded, date-based snapshot buckets so builds can discover reusable entries without Git ancestry or remote directory listings. Refresh cache hits to keep active entries available, support tasks with no output artifacts, and introduce `LUCHTA_SHARED_CACHE_DAYS` while temporarily accepting the deprecated `LUCHTA_SHARED_CACHE_HISTORY` name.
