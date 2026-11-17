The native task executor, frozen check and auth refresh build typed v1 requests
and use the installed release's Rust renderer. Every native baseline case must
retain its frozen profile hash. Missing binaries, timeouts, failed renders and
incompatible headers must raise before an executor command can launch.
