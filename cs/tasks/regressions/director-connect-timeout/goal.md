# director-connect-timeout

On prod-1
(node `m2`, 2026-11-10) the director at its receiver URL was not serving yet,
and each tick's `job-pull` blocked about 30 s: `_director_client` gave
`urlopen` one 30 s timeout for connecting and reading alike.

The check must show:

1. `_director_client.CONNECT_TIMEOUT_S` is at most 3 s.
2. A connect that never completes (simulated SYN drop: `socket.create_connection`
   sleeps its timeout) fails as `DirectorUnreachable` within the connect
   timeout, which is the timeout the connect was given.
3. A director that answers after 4.5 s still succeeds: the read keeps the
   client's long timeout.
4. `ops/bin/job-pull` against an unroutable receiver (192.0.2.1, TEST-NET-1)
   exits 0 with one JSON line naming the error, empty stderr, in under 8 s.

Fails on the previous client (case 2: 30 s, timeout passed 30).
Run: `tasks/regressions/director-connect-timeout/check`, about 11 s.
