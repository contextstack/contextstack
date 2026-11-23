# Turn guidance and repo views

Each request adds a transient turn count. The `prompt` event records the suffix as `turn_guidance`.
The first, last budgeted, and extra requests give the workspace path for relative deliverable paths.
A filename in the frozen goal or a frozen check assertion such as `test -s answer.md` identifies a
deliverable. The last two turns ask for the file, and the final palette keeps just the configured
`fs_write` and `fs_edit` tools if present. The run records final reply text even without the file.
Coding specs (write tool, no named deliverable) keep their palette and get a final instruction to
finish and summarize, with a reminder if `run_check` is available and unused. Turn 1 suggests running
it when the frozen check is substantive.

- A spec with a write tool and no named deliverable gets one planning prompt on the turn after the
  first `run_check` result, or on turn 6 if no result has arrived. It asks which files need to
  change, why, and what edit comes first. The `turn` event records the plan.
- A coding reply without tools continues within budget before a successful edit or while a configured
  check has no passing latest result. Each continuation records a `guidance` event and asks the model to edit and
  run the check. The third consecutive text-only reply ends the run.
- After 12 calls without `fs_write` or `fs_edit`, the guidance asks for a hypothesis, a small edit,
  and a check. If another 12 calls follow that prompt without a write call, the next turn omits
  read tools from both the offered schemas and dispatch. Its guidance asks for the first planned
  edit and a check. `exec`, `fs_edit`, `fs_write`, and `run_check` stay available when configured.
  The next turn restores the full palette. Further reminders can recur, with one pause per streak
  without a write. Either write call resets the count and lets a future nudge escalate again.

Tool failures in `workspace-escape` gain the command or path and recovery guidance. The
`repo-view-guidance` fixture uses a static snapshot and a fixture-only prefix to pin tool dispatch, both
search modes, escape refusals, and the four successive reminders without depending on the host sandbox.
It writes `answer.md` on the last turn, with only the two write schemas offered. `repo_search` accepts
both relative and absolute paths inside the view and visits entries in filename order. It shares the
canonical view root with `fs_read`, which lists directory symlinks that resolve within a readable root.
The fixture receipt pins the terminal trace's `turns`, `tool_errors`, and `deliverables` fields. The
error map counts tool error observations. Deliverable metrics cover explicit relative output filenames
in the frozen goal or check. A file must be new or changed in the workspace at the end of the run. A
missing or unchanged named file records false. An empty map means no filename was identified.

A passing post-turn check, a successful final-turn write tool, or a named deliverable created or changed
during the run yields `done`. The frozen check determines the verdict. Otherwise, for a named
deliverable the runner makes one extra model call when the last budgeted reply contains tool calls,
including refusals and tool errors. The extra turn keeps the same palette. When the palette is
write-only, the reminder tells the model to use the information it already has. An extra turn without a
passing post-turn check, a successful write tool, or a produced deliverable exhausts the turn budget.
Other budget stops still apply. The terminal metrics include `final_turn_retry: true` when the extra
turn runs. A wall-clock deadline during a hook yields `budget:seconds`. A hook that exits by signal
reports that signal. Only the hook's own watchdog expiry uses the watchdog error wording. The focused
turn-boundary regressions exercise both stops and completion before the extra call. The `budget-turns`
fixture pins the extra request and the stop after both calls are refused. Its withheld `fx_tool`
observation explains that read tools are unavailable on the final turn. A text-only final reply can
finish without an extra request. The `budget-zero-unlimited` fixture retains `fx_tool` on its final
request and ends `done`. The `hook-veto-budget-turns` fixture exhausts the budget after the hook veto.
The `repo-view-guidance` fixture ends `done` after the final write. `fs_read`, `fs_write`, `fs_edit`,
and `exec` accept absolute paths resolving inside the workspace. The `fs_write` and `fs_edit` workspace
escape refusals keep the `path escapes workspace: <path>` prefix and append the absolute workspace path
and a relative filename example. Repository view refusals also retain their read-only explanation.

For a spec with a named deliverable, the last budgeted turn and the extra turn send a named
`tool_choice` when one write tool is offered, or `"required"` when both are offered. Earlier requests
and requests with an empty write palette omit the field. The scripted `budget-turns` fixture offers
`fs_write` and pins the named choice on both requests even though the fake model keeps calling the
removed `fx_tool`. The `repo-view-guidance` fixture pins `"required"`. Unknown-tool observations list
the names offered on the current turn. They suggest the closest available name by edit distance. For
`list` and `list_dir`, they suggest using `fs_read` on a directory if it is available. The
`tool-not-in-palette` fixture pins the new observation.

Director-built harness specs for coder and researcher now offer `request_action` on ordinary tool turns.
The final write-only turn keeps its existing tool choice. The characterisation fixtures use explicit
palettes. Approved commands have a 60-second execution bound within the 120-second tool timeout so the
approval flow can report the outcome. Trace-e2e exercises approval, escalation, and repeated requests,
and its researcher fixture expects the new tool.

`exec` accepts a shell string as `cmd` or as the entire argument, running it with `/bin/sh -c` inside
the tool sandbox. The assistant history replays a bare string as a `cmd` field for providers that
require an arguments object. Direct argv still uses the workspace and repository-view path checks. The
repo-view fixture's shell string now reaches the shell and reports the missing workspace-relative
directory. `repo_search` searches the lane workspace when there is no snapshot.

Coder palettes include `fs_edit`, `repo_search`, and `run_check`. The latter uses the frozen check from
this run and returns its exit code and up to 8 KiB of combined output under the check window and the run
deadline. Its observation counts toward the tool budget. The finalizer grades independently. Malformed
OpenAI-compatible tool arguments become an error observation paired with the original call ID and leave
valid sibling calls available.
