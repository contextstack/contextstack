# Terminal dispatch rows release scheduler seats

The scheduler counts only unclosed running job identities. A historical
running row followed by a terminal row cannot occupy a seat merely because
the operating system later reuses its PID for an unrelated live process.
