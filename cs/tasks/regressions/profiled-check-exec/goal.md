# OpenCode Go's task check must run

An opencode-go lane finished its CLI work, then the frozen task check
exited 126. The shell could not open the stimulus check: Operation not
permitted. The same check file runs after cursor-agent and codex.

The profiled check sandbox allows the stimulus directory only as an extra
rule. The estate read deny is applied later and covers that path, because
the stimulus directory is not a read root. cursor-agent and codex pass the
stimulus directory as a read root, so the deny does not apply to it.

The OpenCode Go agent keeps its login-store deny. The check gains read
access only to its stimulus directory and its tmp, the same grant the
other CLIs already use.
