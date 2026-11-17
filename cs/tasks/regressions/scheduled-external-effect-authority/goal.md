# Scheduled external-effect authority

An external mutation requested by a schedule must be named both by the
schedule and its active grant, survive enqueue and dispatch as structured
metadata, and become process authority only for that exact child. The worker
must remove an inherited `LAB_VIDEO_MUTATION` value from every child and set
it to `1` only when the job carries the granted
`youtube_playlist_mutation` effect.

The Video Monitor capture schedule is the sole current holder. Judgment and
other corpus schedules remain unable to mutate YouTube even if the scheduler
service environment is contaminated.
