# lane-policy

The protected agent-config check looks only at what the lane changed since it branched (merge-base), so files that only main changed later never cause a refusal; a lane that edits a protected path is still refused. Review 2026-11-15.
