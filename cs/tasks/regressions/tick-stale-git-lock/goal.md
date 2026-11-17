Tick removes a stale lab `.git` lock before the estate-commit pass.
A lock nobody holds must never stop a commit pass.

The fixture loads the live tick with LAB_STATE_ROOT set to a throwaway
directory first and asserts lab/ledger's dispatch.jsonl and tick-heartbeat
are unchanged: it once appended a real seat_snapshot on every check
(P5).
