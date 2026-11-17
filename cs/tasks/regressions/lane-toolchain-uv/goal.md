Lane sandbox grants read/exec to the declared `uv` binary and its managed Python install directory only; `UV_*` writes go to the run tmp; sibling paths under `~/.local` stay unreadable.

Demand: cs regressions need `uv` on machines where the node service PATH lacks it. Lanes must not read all of `~/.local` or write the host cache.
