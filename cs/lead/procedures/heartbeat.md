# Heartbeat

1. Identify the project in the heartbeat message. Check active jobs and reconcile finished work with its backlog items.
2. Use `backlog` with `action=next` to find the highest-priority unblocked item. If none is ready, report that and finish.
3. Read relevant decisions. Advance the item within its approved scope. Check for an existing job before dispatching and use `find_task` if a submission's outcome is uncertain.
4. Update the item with its status, linked jobs, and blockers. Read `owner-decisions` if a choice needs the owner.
5. Report what advanced and finish the heartbeat turn.
