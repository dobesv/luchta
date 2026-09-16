---
luchta: minor
---
# Add `--output plain` for supervisors that read output line by line

Overmind, Foreman, and Hivemind run each process in a pty and then forward its
bytes to a line-buffered reader. Luchta saw a capable terminal and drew one
status line rewritten with a carriage return, never terminated by a newline, so
the supervisor buffered every refresh instead of showing it. A long build looked
like it printed nothing until it finished.

`--output plain`, or `LUCHTA_OUTPUT=plain`, keeps the append-only status lines
every five seconds plus the final summary even when the terminal could redraw in
place. `LUCHTA_OUTPUT` also accepts `default` and `summary`; the flag wins when
both are given, and an unrecognized value is rejected rather than ignored.
