---
host_only: requires applying a fresh macOS Seatbelt profile and binding fake localhost listeners
---

# Land-check sandbox

Run a hostile child under the rendered Seatbelt profile. It must be able to
write only to its granted build directories, use a random localhost port and
use Unix sockets under scratch. It must be denied writes to HOME and the
main checkout, reads of stand-in credentials, and access to reserved ports,
external IP addresses, and Unix sockets outside scratch.
