---
luchta: minor
---
# Add advisory task cache files

Allow cache-enabled tasks to declare package-local `cacheFiles` that are
restored as performance-only warm state without affecting task skipping,
output hashes, or downstream invalidation.
