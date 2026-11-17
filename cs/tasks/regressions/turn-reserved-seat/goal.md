Director conversation turns start immediately alongside running lane jobs, including
while ordinary dispatchers are occupied. One extra seat is reserved for Director-owned
conversation turns; lane and batch jobs, including forged bundle metadata, cannot
use it. Running rows retain turn identity and seat accounting. The same conversation
never overlaps, and critical memory pressure prevents admission until recovery.

Reproduce the workstation release 20261112-6cdd4f074 observation: job
01M77SP05Y0ZNKS5AA0FX7AE3E was queued at 11:53:24Z and started at 11:58:47Z.
node-listen suppressed both offered-job kicks and one-second retries while work
ran; its previous tick also remained alive for the job's lifetime. The periodic
launchd service (StartInterval=60) cannot launch another instance while its tick
executes a job. Together these blocked admission behind the two lane jobs.
The dispatch flock is released before execution; only a destination resource
flock lasts through execution. occupied() is telemetry, not a seat cap, and
admission_locked=true records successful lock acquisition, not a stuck lock.
Use a fake Director, fake clock and stub executors through real intake, listener
and tick admission; no production state or models.
