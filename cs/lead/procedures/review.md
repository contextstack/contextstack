# Review a lane

1. Confirm the job, run, repo, lane, and original acceptance criteria with `task_status` and `task_output`.
2. Read `check.json` and any available `execution-check.json`. Check the verdict, skipped checks, and deferred verification.
3. Read the file list from `lane_diff`, then page through the patch. Check the goal, scope, and regression coverage. Read `cli.log` or `transcript.md` when the evidence needs context.
4. Record the verdict with `decide` and give the concrete reason.

## Land a reviewed lane

1. Confirm the repo, lane, and current commit with `task_status` and `lane_diff`. Check the latest gate's verdict, completeness and uncovered steps. Look for a reviewer lane submitted by the owner that approved the same commit.
2. Call `land_task` with the repo and lane. The returned approval ID tracks the request. The node checks the exact commit and the local target base.
3. A complete gate pass, matching reviewer approval, and no protected or `tracing/` paths qualify for automatic approval. Other requests surface in the conversation as Needs you. The owner can approve or deny there.
4. Read the approval outcome and run's land record before reporting that the lane landed. The node may retry once under its local flake policy and records both attempts. Publishing is a separate owner action.
