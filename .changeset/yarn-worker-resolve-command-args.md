---
luchta: patch
---
Fix `luchta-yarn-worker` incorrectly pruning tasks whose `command` carries
extra arguments after the script name (for example `"args --flag 'two
words'"`). Resolution now checks only the first token against the package's
declared scripts, matching how the script actually runs.
