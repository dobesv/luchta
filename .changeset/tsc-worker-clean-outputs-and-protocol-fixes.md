---
luchta: patch
---
Fix three tsc-worker defects: CleanOutputs now honors declarationDir and refuses to delete outside the package (and actually runs — it was a silent no-op); protocol write errors surface as a non-zero worker exit instead of a silent empty result; ResolveInputs resolves package-specifier `extends` under Yarn PnP so cache inputs are complete.
