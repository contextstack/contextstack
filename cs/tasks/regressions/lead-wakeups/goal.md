# lead-wakeups

Hosted lead child-lane wake-ups (Design.HostedLead lane 6): one batched nudge per watched job end, wait for the session reply, record it on the Director as a wake turn, resync after SSE drop.

Socket watch registrations require the current serve token; forged registrations must not inject nudges (#39).
