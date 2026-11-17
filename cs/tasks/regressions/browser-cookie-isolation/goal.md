# Browser cookie isolation

Authenticated browser state is operator-owned runtime state, never corpus
evidence. Its profile and session files live outside run workspaces and task
setup trees. Cookie values may not enter transcripts, evidence, runtime spill,
or harvested commits. Cookie-imported Google browsing and unauthenticated
public browsing use distinct browser state and daemon partitions, so the
cookie-domain guard on one cannot block or contaminate the other.
