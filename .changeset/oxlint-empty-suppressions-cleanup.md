---
luchta: patch
---
Delete the oxlint suppressions file when all suppressions are removed instead of leaving behind an empty JSON object. Empty-object detection is recursive to handle nested and invalid suppression data.
