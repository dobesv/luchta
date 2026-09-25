---
luchta: patch
---
Stop `luchta watch` from rescanning in a loop on Linux: the watcher no longer subscribes to file open/read events, which notify 9 enabled by default and which the build's own tools generated faster than the inotify queue could drain. Worker watchers likewise no longer restart when a watched file is merely read.
