# land-record

`cs land` squash-merges a task lane into main and then forgets to tell the
Director, so a client cannot show which lanes landed. After `land()` has
decided, report the outcome. Do not change the gate, the merge, or cleanup.

Against temp repos and a fake Director (node protocol plus the Client API):

1. A green land posts `state: landed` with the new commit on main. A red
   check posts `state: failed` and the first FAIL line. A dirty main posts
   `state: refused` and the refusal text. Each post carries one
   Idempotency-Key, the node token, and `landed_by` / `landed_at`.
2. `--job` selects that job's run. Without it, the newest finished job whose
   meta names this repo and lane is used; an unfinished job and a different
   repo or lane are not.
3. When no finished job matches, the land still completes and a warning says
   it couldn't be recorded.
4. An unreachable Director leaves the landing outcome as it was.
   `cs land --json` then has `recorded: false` and `record_error`; a recorded
   land has `recorded: true` and `land_id`.
