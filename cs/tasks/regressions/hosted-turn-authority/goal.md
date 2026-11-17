# Hosted turn authority (#38, independent security verification at 2e2f5b77c)
Only a Director conversation turn for this host may use the hosted lead path.
Ordinary jobs claiming hosted-lead or tasks/turn stop before workspace setup.
Hosted checks use the ordinary sandbox; the host retains its credential path,
and the session sandbox can read only its own persistent HOME.
