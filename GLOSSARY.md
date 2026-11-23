# ContextStack

ContextStack lets one developer delegate coding work to AI agents running on their own machines, watch it happen, and keep or discard the result.

## Delegating work

**Task**:
A piece of work someone asks for, stated as what to do plus how to tell it's done. It stays open until it succeeds or is discarded.
_Avoid_: Intent, ticket, work order, issue

**Goal**:
A larger outcome that is broken down into several tasks. (Not built yet.)
_Avoid_: Intent, epic, mission, project (a project is what work is charged to)

**Lane**:
The isolated place where an agent does one task: its own branch and working copy, kept apart from everything else until the result is landed or discarded.
_Avoid_: Branch, worktree (each is only part of a lane), sandbox

**Task lane**:
A task together with its lane, as one thing the user starts, watches and lands.
_Avoid_: Job, session, thread, mission

**Land**:
To accept a lane's result into the main line of the repo, after its checks pass.
_Avoid_: Merge (a git operation that landing performs), ship, apply

**Discard**:
To drop a lane's result without landing it.
_Avoid_: Cancel (which stops work in progress), delete

**Check**:
The test that decides whether a task is done.
_Avoid_: Gate, verifier, acceptance test

**Job**:
One attempt by one agent to do a task, on one machine. A retry, on the same or another model, is another job in the same task lane.
_Avoid_: Run (a job's record), lane, work order, new task

**Run**:
The record of what happened during one job: what the agent did, what it cost, and its verdict.
_Avoid_: Trace, execution, attempt

**Decision log entry**:
A record of what a project chose, why it chose it, what alternatives it considered and what would make it reconsider.

## Agents and conversations

**Agent**:
An AI coding tool that does a job, such as Claude Code, Codex or Cursor, signed in with the user's own subscription or key.
_Avoid_: Bot, droid, model (an agent uses a model), worker

**Agent profile**:
A named role an agent plays in a job (coder, reviewer, researcher, verifier), with its instructions, model and limits.
_Avoid_: Persona, agent type, template

**Reviewer**:
An agent profile that reads a lane's result and reports findings without changing anything.
_Avoid_: Auditor, critic

**Lead**:
The agent the user talks to, which plans the work and starts, watches and judges task lanes on the user's behalf.
_Avoid_: Orchestrator, manager, chief of staff, hosted lead (the lead is always hosted)

**Conversation**:
An ongoing exchange between the user and an agent, kept on the Director.
_Avoid_: Thread, chat, session

**Message**:
What the user sends in a conversation.
_Avoid_: Prompt, input, turn

**Reply**:
What the agent sends back to a message.
_Avoid_: Response, answer, output, turn

## Machines and installs

**Director**:
The one place that holds the user's tasks, conversations, budgets and records, and hands jobs to machines.
_Avoid_: Server, controller, hub, coordinator

**Machine**:
A computer that runs jobs for the Director: it pulls jobs, runs them in the sandbox, finishes them and lands them. In code and in the node protocol it is still called a node; that rename comes after 0.1.0.
_Avoid_: Node (in anything a user reads), worker, runner, host

**Release**:
One installed version of ContextStack on a machine; older releases are kept for rollback.
_Avoid_: Build, bundle (what a release is installed from), version

**Estate**:
Everything one ContextStack install owns on a machine: its configuration, records and runs.
_Avoid_: Instance, environment, workspace

**Action approval**:
A Director record for one exact command that needs added sandbox permissions. An independent reviewer can authorize one execution or send it to the owner’s needs-you list.

## Project backlog

**Backlog item**

A record of project work that can be next, in progress (`now`), blocked, done, or dropped. Dropping an item keeps its record and revision history.

**Blocker**

A dependency on the owner, a job, another backlog item, or an external event. An owner blocker appears in needs-you and can be flagged to pause the project's heartbeat.

**Next unblocked item**

The first item in priority order with status `next` and all blockers cleared.

**Heartbeat**

An optional project schedule that starts a turn in a lead conversation to advance the backlog. Each heartbeat waits until the previous heartbeat turn finishes.
