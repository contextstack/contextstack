# Local-coder outage deferral

When the DGX endpoint is unavailable, a due local-coder schedule must
remain due: do not enqueue it and do not append its slot to the fire
ledger. An already-pending local-coder job must likewise remain pending
until the endpoint is healthy.

This regression records the 2026-09-02 power outage: the laptop and
scheduler survived, the DGX restarted, and the laptop-side SSH forward
did not. Several scheduled trials were voided by transport failure even
though no corpus experiment had actually run.
