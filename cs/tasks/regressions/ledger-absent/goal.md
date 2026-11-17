A checkout whose ledger has left for the state root stays checkable and never
grows a new lab/ledger (P4 and P6).

In a fixture lab with no ledger/, check-red-gate refuses the same way; estate-identity reports a skip; a
ledger-relative setup link resolves through LAB_STATE_ROOT (and through
lab/ledger when it is unset). scripts/check's rule 10 fails if lab/ledger
reappears once HEAD tracks none of it (exercised by the deletion commit).
Run: `tasks/regressions/ledger-absent/check`, about 1 s.
