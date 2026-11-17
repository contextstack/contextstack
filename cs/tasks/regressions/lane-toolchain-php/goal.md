A node-declared PHP toolchain runs through PATH in the executor's macOS
sandbox. Its wrapper and real binary directories remain read-only; a sibling
under HOME/Library remains unreadable. Without --darwin-probe, skip.
Discovery follows symlinks and static shell launcher paths, discovers only PHP
and Composer when no config exists, and honors explicit toolchains.json.
Demand: coding lanes cannot run client-web tests with Herd PHP.
