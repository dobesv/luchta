---
luchta: minor
---
yarn worker tasks now always run through `yarn` instead of executing resolved `package.json` script bodies directly. When a worker task omits `command` or sets it to blank whitespace, Luchta now defaults that worker command to task name and always sends a workspace hint so worker runs `yarn <task>` at root or `yarn workspace <name> <task>` for packages.
