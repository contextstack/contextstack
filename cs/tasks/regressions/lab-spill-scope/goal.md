# Spill scan scope

Tracked `.lab-*` spill is refused in the lab repo, destination
repos, and run workspaces newer than the last prune. Pre-prune
workspaces are covered by the prune itself and are not walked.
