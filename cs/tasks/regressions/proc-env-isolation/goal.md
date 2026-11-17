# Process environment isolation

Demand: P0, sixth verification pass: a lane read a decoy node token through
macOS KERN_PROCARGS2 under the real executor profile.

`check` verifies direct node service launches, node.env parsing without
exports, custom node token names, and node-side gateway bearer loading without
environment fallback. Generated plists and units must contain no nonempty
environment key ending in _TOKEN, _SECRET or _KEY.

On a macOS host, run `check --darwin-probe`. A temporary install's node.env
contains only a random decoy. The exact node_args_env command and environment
launch a stand-in op that loads the settings in memory and spawns a child.
Both pids are probed through ps and ctypes sysctl KERN_PROCARGS2 under the real
executor profile; neither may leak. The old shell-source-and-exec launch must
leak under that same profile. Only booleans are printed.
Nested lanes cannot apply Seatbelt or create node.env; offline checks use a
non-secret fixture filename. No installed services or credentials are read.
