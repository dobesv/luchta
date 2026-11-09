---
luchta: patch
---
Cache restores stage files under `.luchta/tmp` and create no staging folder for empty restores. Workers and Luchta's input and output walks skip entries that vanish mid-walk.

Fixes #950.
