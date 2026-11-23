Tick removes a stale code_root `.git` lock.

The fixture loads the live tick with CS_STATE_ROOT set to a throwaway
directory first and asserts cs/ledger's dispatch.jsonl and tick-heartbeat
are unchanged: it once appended a real seat_snapshot on every check
(P5).
