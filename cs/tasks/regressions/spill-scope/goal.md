# Spill scan scope

Tracked `.cs-*` spill is refused in the code_root repo, destination
repos, and run workspaces newer than the last prune. Pre-prune
workspaces are covered by the prune itself and are not walked.
