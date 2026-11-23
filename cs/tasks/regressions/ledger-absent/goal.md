A checkout whose ledger has left for the state root stays checkable and never
grows a new cs/ledger (P4 and P6).

In a fixture code_root with no ledger/, check-red-gate refuses the same way; estate-identity reports a skip; a
ledger-relative setup link resolves through CS_STATE_ROOT (and through
cs/ledger when it is unset). scripts/check's rule 10 fails if cs/ledger
reappears once HEAD tracks none of it (exercised by the deletion commit).
Run: `tasks/regressions/ledger-absent/check`, about 1 s.
