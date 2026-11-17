"""Client side, step 6 (CLIENT-API.md §11; outline §7): conversations. A thread of turns, each an ordinary job; one
active turn with a queue behind it; stop and resume; replies as they are written; compaction that keeps the whole
thread, with summaries made by ordinary jobs. A protocol-2 stand-in node (harness/node2.py) plays the node and
returns canned replies in the run's messages.jsonl, as the Rust runner records them."""
import json

from harness.client import new_key
from harness.node2 import delta, stop
from harness.run import case
from harness.wire import run_fixture, sha, terminal_fixture

from cases.client import device, owner, refused

C = "client"
# The target serves `--context-window dspec-tiny=600` (harness/targets.py), so a short thread compacts.
TINY = "dspec-tiny"
SPEC = """id: dspec-conversation
model: %s
system: prompts/p.md
context: [conversation/context.md]
tools: []
mcp: []
budget: {turns: 4, tool_calls: 0, usd: 0, seconds: 60}
tasks: [tasks/turn]
trials: 1
hypothesis: fixture a conversation fixture
"""
PROMPT = b"You are a fixture.\n"


def bundle(model="dspec-model", spec=None):
    return {"spec.yaml": (spec or SPEC % model).encode(), "prompts/p.md": PROMPT}


def upload(c, files):
    for data in files.values():
        a = c.call("PUT", "objects/" + sha(data), raw=data)
        if a.status != 200:
            raise RuntimeError("PUT objects: %r" % a)
    return [{"path": p, "sha256": sha(d), "mode": 420} for p, d in sorted(files.items())]


def create_body(c, node, model="dspec-model", files=None, **over):
    body = {"node": node.name, "spec": "spec.yaml", "files": upload(c, files or bundle(model)), "title": "chat"}
    body.update(over)
    return body


def created(ctx, c, node, model="dspec-model", **over):
    a = c.call("POST", "conversations", body=create_body(c, node, model, **over), key=new_key("conv"))
    ctx.expect(a.status == 201 and a.json["conversation"]["state"] == "idle", "POST conversations is 201", a)
    return a.json["conversation"]


def post(ctx, c, conv, message, key=None):
    a = c.call("POST", "conversations/%s/turns" % conv["conversation_id"], body={"message": message},
               key=key or new_key("turn"))
    ctx.expect(a.status == 201, "POST turns is 201", a)
    return a.json["turn"]


def thread(c, conv):
    return c.call("GET", "conversations/%s/thread" % conv["conversation_id"]).json


def turn(c, ident):
    return c.call("GET", "turns/" + ident).json


def job_files(node, job_id):
    """The job's bundle as the node fetches it: {path: bytes}."""
    return {f["path"]: node.call("GET", "objects/" + f["sha256"]).body for f in node.job(job_id)["files"]}


def reply(ctx, node, job_id, text, run_id=None):
    """The node accepts the job, runs it, and reports a run whose messages.jsonl ends with `text`, then the job."""
    node.accept(job_id)
    run_id = run_id or "%s-%s" % (ctx.serial_tag(), job_id[-8:].lower())
    messages = "\n".join(json.dumps(e) for e in ({"event": "header"}, {"event": "turn", "turn": 1, "text": text},
                                                 {"event": "turn", "turn": 2, "text": ""})) + "\n"
    run = run_fixture(run_id, overrides={"messages.jsonl": messages.encode()})
    for data in run["files"].values():
        ctx.expect(node.call("PUT", "objects/" + sha(data), raw=data).status == 200, "PUT objects")
    a = node.call("POST", "envelopes", body={"envelope": run["envelope"], "row_line": run["row_line"]})
    ctx.expect((a.json or {}).get("status") == "acked", "run_finalized acked", a)
    row = json.dumps({"state": "done", "director_job": job_id, "run_id": run_id})
    term = {"envelope_version": 1, "event_id": "job_terminal:" + job_id, "job_id": job_id, "files": [],
            "envelope_sha256": sha(b""), "row_sha256": sha(row)}
    a = node.call("POST", "envelopes", body={"envelope": term, "row_line": row})
    ctx.expect((a.json or {}).get("status") == "acked", "job_terminal acked", a)
    return run_id


def open_jobs(node):
    return [j["job_id"] for j in node.call("GET", "jobs").json["jobs"]]


@case(C, "conversation", "conversation-turn-becomes-job")
def _(ctx):
    """A turn is an ordinary job: the chosen spec bundle plus the generated context, goal and check. The node sees
    nothing of the conversation. The turn records the conversation revision and spec version it used; the run's
    final reply becomes the turn's reply."""
    n, c = ctx.node2(), owner(ctx)
    conv = created(ctx, c, n)
    ctx.expect(conv["model"] == "dspec-model" and conv["compaction"]["window_source"] == "default"
               and conv["compaction"]["context_window"] == 32768, "the compaction budget", conv)
    t = post(ctx, c, conv, "Hello there.")
    ctx.expect(t["state"] == "running" and t["seq"] == 1 and t["job_id"] in open_jobs(n), "it starts at once", t)
    job = n.job(t["job_id"])
    # An ordinary job plus Director-owned turn identity (from the turns table, never caller meta),
    # so the node can tell a real conversation turn from an intent that only claims to be one.
    rest = {k: v for k, v in job.items() if k not in ("files", "conversation_id", "turn_id")}
    ctx.expect(job["task"] == "tasks/turn" and job["spec"] == "spec.yaml" and job["meta"] == {}
               and job.get("conversation_id") == conv["conversation_id"] and job.get("turn_id") == t["turn_id"]
               and "conversation" not in json.dumps(rest),
               "an ordinary job carrying its Director-owned turn identity", job)
    files = job_files(n, t["job_id"])
    ctx.expect(files["tasks/turn/goal.md"] == b"Hello there." and files["spec.yaml"] == bundle()["spec.yaml"]
               and b"first message" in files["conversation/context.md"]
               and b"exit 0" in files["tasks/turn/check"], "spec bundle plus generated files", sorted(files))
    ctx.expect(t["context"]["spec_version"] == conv["spec_version"]
               and isinstance(t["context"]["conversation_revision"], int)
               and t["context"]["summary_id"] is None, "the turn records what it used", t["context"])
    ctx.expect(c.call("GET", "jobs/" + t["job_id"]).json["conversation_id"] == conv["conversation_id"],
               "the client sees the job's conversation")
    reply(ctx, n, t["job_id"], "General Kenobi.")
    done = turn(c, t["turn_id"])
    ctx.expect(done["state"] == "done" and done["reply"] == "General Kenobi." and done["run_id"], "the reply", done)
    th = thread(c, conv)
    ctx.expect(th["conversation"]["state"] == "idle" and [x["reply"] for x in th["turns"]] == ["General Kenobi."],
               "the thread", th)
    listed = c.call("GET", "conversations?node=" + n.name).json["conversations"]
    ctx.expect([x["conversation_id"] for x in listed] == [conv["conversation_id"]], "listed by node", listed)


@case(C, "conversation", "conversation-queue-two-clients")
def _(ctx):
    """Messages from two devices queue behind the active turn: one job at a time; the queued turn's context is
    assembled when it becomes eligible, so it carries the reply before it, and a newer conversation revision."""
    n, c, phone = ctx.node2(), owner(ctx), device(ctx)
    conv = created(ctx, c, n)
    first = post(ctx, c, conv, "First, from the laptop.")
    second = post(ctx, phone, conv, "Second, from the phone.")
    ctx.expect(first["state"] == "running" and second["state"] == "queued" and second["job_id"] is None
               and second["seq"] == 2, "the second waits", (first, second))
    ctx.expect(open_jobs(n) == [first["job_id"]], "one job at a time", open_jobs(n))
    now = c.call("GET", "conversations/" + conv["conversation_id"]).json["conversation"]
    ctx.expect(now["active_turn_id"] == first["turn_id"] and now["queued_turn_ids"] == [second["turn_id"]],
               "the queue", now)
    reply(ctx, n, first["job_id"], "REPLY-ONE")
    second = turn(c, second["turn_id"])
    ctx.expect(second["state"] == "running" and second["job_id"], "the queue moves", second)
    context = job_files(n, second["job_id"])["conversation/context.md"].decode()
    ctx.expect("First, from the laptop." in context and "REPLY-ONE" in context
               and "Second, from the phone." not in context, "the prior reply is in the context", context)
    ctx.expect(second["context"]["conversation_revision"] > first["context"]["conversation_revision"]
               and second["posted_by"] == phone.credential_id, "a newer revision; who posted", second)


@case(C, "conversation", "conversation-stop-and-resume")
def _(ctx):
    """Stop holds the queue at once and requests cancel of the active turn (a protocol-2 control message); the
    cancel stays requested until the node reports. Resume releases the queue."""
    n, c = ctx.node2(), owner(ctx)
    conv = created(ctx, c, n)
    first = post(ctx, c, conv, "A long question.")
    second = post(ctx, c, conv, "A follow-up.")
    a = c.call("POST", "conversations/%s/stop" % conv["conversation_id"], body={}, key=new_key("stop"))
    ctx.expect(a.status == 202 and a.json["conversation"]["held"] is True and a.json["conversation"]["state"] == "held",
               "held at once", a)
    cancel = a.json["cancel"]
    ctx.expect(cancel and cancel["state"] == "requested" and cancel["pending"] is True
               and cancel["job_id"] == first["job_id"], "cancel requested", cancel)
    ctl = [m for m in n.control().json["control"] if m["kind"] == "cancel"]
    ctx.expect([m["body"]["job_id"] for m in ctl] == [first["job_id"]], "the node can pick it up", ctl)
    ctx.expect(turn(c, first["turn_id"])["cancel"]["state"] == "requested", "still requested before the node reports")
    n.report(ctl[0]["control_id"], "applied", "stopped")
    term = terminal_fixture(first["job_id"], state="error")
    ctx.expect(n.call("POST", "envelopes", body=term).json["status"] == "acked", "job over")
    stopped = turn(c, first["turn_id"])
    ctx.expect(stopped["state"] == "stopped" and stopped["cancel"]["outcome"] == "stopped", "stopped", stopped)
    ctx.expect(turn(c, second["turn_id"])["state"] == "queued" and open_jobs(n) == [], "the queue is held")
    a = c.call("POST", "conversations/%s/resume" % conv["conversation_id"], body={}, key=new_key("resume"))
    ctx.expect(a.status == 200 and a.json["conversation"]["held"] is False, "resumed", a)
    second = turn(c, second["turn_id"])
    ctx.expect(second["state"] == "running" and open_jobs(n) == [second["job_id"]], "the queue moves", second)
    context = job_files(n, second["job_id"])["conversation/context.md"].decode()
    ctx.expect("A long question." in context and "the turn was stopped" in context, "the stop is in the thread", context)


@case(C, "conversation", "conversation-live-events-tagged")
def _(ctx):
    """The active turn's live events reach the stream tagged with the conversation and turn, also on a stream
    opened for that conversation only; the finished run's reply replaces the partial text."""
    n, c = ctx.node2(), owner(ctx)
    conv, other = created(ctx, c, n), created(ctx, c, n)
    s = c.stream()
    only = c.stream_for(conv["conversation_id"])
    try:
        for st in (s, only):
            st.until(lambda name, _d: name == "hello")
        t = post(ctx, c, conv, "Tell me a story.")
        n.accept(t["job_id"])
        n.events("live-" + ctx.serial_tag(), t["job_id"], [delta(1, "Once "), delta(2, "upon"), stop(3)])
        for st in (s, only):
            got = []
            while len(got) < 3:
                e, _ = st.until(lambda name, _d: name == "run_event")
                ctx.expect(e is not None, "run events arrive", got)
                got.append(e[1])
            ctx.expect(all(g["conversation_id"] == conv["conversation_id"] and g["turn_id"] == t["turn_id"]
                           for g in got) and [g["seq"] for g in got] == [1, 2, 3], "tagged, in order", got)
        # Another conversation's turn does not reach the filtered stream.
        o = post(ctx, c, other, "Elsewhere.")
        n.events("live-o-" + ctx.serial_tag(), o["job_id"], [delta(1, "nope")])
        e, _ = s.until(lambda name, d: name == "run_event" and d.get("conversation_id") == other["conversation_id"])
        ctx.expect(e is not None, "the unfiltered stream carries it")
        reply(ctx, n, t["job_id"], "Once upon a time, the end.")
        e, seen = only.until(lambda name, d: name == "change" and d["kind"] == "turn" and d["record"]["state"] == "done")
        ctx.expect(e and e[1]["record"]["reply"] == "Once upon a time, the end.", "the reply replaces the partial", e)
        ctx.expect(all((d or {}).get("record", d).get("conversation_id") == conv["conversation_id"]
                       for name, d, _ in seen if name in ("change", "run_event")), "only this conversation", seen)
    finally:
        s.close()
        only.close()


def converse_until_summary(ctx, c, n, conv):
    """Post and answer turns until one waits for a summary. Returns (turns posted, the waiting turn)."""
    posted = []
    for i in range(1, 12):
        t = post(ctx, c, conv, "Question %d: %s" % (i, "q" * 200))
        posted.append(t)
        if t["state"] == "summarising":
            return posted, t
        ctx.expect(t["state"] == "running", "a turn runs or waits for a summary", t)
        reply(ctx, n, t["job_id"], "Answer %d: %s" % (i, "a" * 200))
    raise AssertionError("no compaction after 11 turns")


@case(C, "conversation", "conversation-compaction-keeps-history")
def _(ctx):
    """Past the threshold a summary of an identified older portion is made by an ordinary job, then the turn sends
    the summary plus the recent turns whole. The whole thread stays readable."""
    n, c = ctx.node2(), owner(ctx)
    conv = created(ctx, c, n, TINY)
    ctx.expect(conv["compaction"]["context_window"] == 600 and conv["compaction"]["window_source"] == "setting",
               "the window is a setting", conv["compaction"])
    posted, waiting = converse_until_summary(ctx, c, n, conv)
    ctx.expect(len(posted) > 2, "the first turns send the whole thread", len(posted))
    summary = thread(c, conv)["summaries"][-1]
    k = summary["through_seq"]
    ctx.expect(summary["state"] == "running" and 1 <= k < waiting["seq"] - 1 and summary["based_on"] is None
               and open_jobs(n) == [summary["job_id"]], "a summary job for turns 1-k", summary)
    files = job_files(n, summary["job_id"])
    goal, context = files["tasks/turn/goal.md"].decode(), files["conversation/context.md"].decode()
    ctx.expect(("turns 1-%d" % k) in goal and "Question 1:" in context and ("Question %d:" % (k + 1)) not in context,
               "the summary job sees the portion it covers", (goal, context[:300]))
    reply(ctx, n, summary["job_id"], "SUMMARY-ONE of turns 1-%d" % k)
    waiting = turn(c, waiting["turn_id"])
    ctx.expect(waiting["state"] == "running" and waiting["context"]["summary_id"] == summary["summary_id"]
               and waiting["context"]["summary_through_seq"] == k and waiting["context"]["whole_from_seq"] == k + 1,
               "the turn runs on the summary", waiting)
    context = job_files(n, waiting["job_id"])["conversation/context.md"].decode()
    ctx.expect("SUMMARY-ONE" in context and "Question 1:" not in context and ("Question %d:" % (k + 1)) in context,
               "summary plus recent turns whole", context)
    reply(ctx, n, waiting["job_id"], "Answer after compaction.")
    th = thread(c, conv)
    ctx.expect([t["seq"] for t in th["turns"]] == list(range(1, waiting["seq"] + 1))
               and th["turns"][0]["message"].startswith("Question 1:") and th["turns"][0]["reply"].startswith("Answer 1:"),
               "the full thread is kept", [t["seq"] for t in th["turns"]])


@case(C, "conversation", "conversation-summary-provenance-and-redo")
def _(ctx):
    """A summary records the job and run that made it; a bad one is redone by a new job for the same portion, and
    later turns use the redone one."""
    n, c = ctx.node2(), owner(ctx)
    conv = created(ctx, c, n, TINY)
    _, waiting = converse_until_summary(ctx, c, n, conv)
    first = thread(c, conv)["summaries"][-1]
    sid = first["summary_id"]
    refused(ctx, c.call("POST", "conversations/%s/summaries/%s/redo" % (conv["conversation_id"], sid), body={},
                        key=new_key()), 409, "summary_running", "redo while it runs")
    run = reply(ctx, n, first["job_id"], "A BAD SUMMARY")
    first = c.call("GET", "summaries/" + sid).json
    ctx.expect(first["state"] == "done" and first["run_id"] == run and first["text"] == "A BAD SUMMARY"
               and c.call("GET", "runs/" + run).json["conversation_id"] == conv["conversation_id"],
               "provenance: the job and run that made it", first)
    waiting = turn(c, waiting["turn_id"])
    reply(ctx, n, waiting["job_id"], "Answer on the bad summary.")
    key = new_key("redo")
    a = c.call("POST", "conversations/%s/summaries/%s/redo" % (conv["conversation_id"], sid), body={}, key=key)
    ctx.expect(a.status == 202 and a.json["summary"]["redoes"] == sid
               and a.json["summary"]["through_seq"] == first["through_seq"] and a.json["summary"]["state"] == "running",
               "a new summary job for the same portion", a)
    again = c.call("POST", "conversations/%s/summaries/%s/redo" % (conv["conversation_id"], sid), body={}, key=key)
    ctx.expect(again.status == 200 and again.json["summary"]["summary_id"] == a.json["summary"]["summary_id"],
               "the key replays", again)
    redo = a.json["summary"]
    reply(ctx, n, redo["job_id"], "A GOOD SUMMARY")
    t = post(ctx, c, conv, "And now?")
    ctx.expect(t["state"] == "running", "the next turn runs", t)
    context = job_files(n, t["job_id"])["conversation/context.md"].decode()
    ctx.expect(t["context"]["summary_id"] == redo["summary_id"] and "A GOOD SUMMARY" in context
               and "A BAD SUMMARY" not in context, "the redone summary is used", (t["context"], context))
    th = thread(c, conv)
    ctx.expect([s["summary_id"] for s in th["summaries"]][:2] == [sid, redo["summary_id"]]
               and th["summaries"][0]["text"] == "A BAD SUMMARY", "both kept, traceable", th["summaries"])


@case(C, "conversation", "conversation-idempotency-and-conflicts")
def _(ctx):
    """Every conversation write carries a key: the same key and body replays the record; another body under it is a
    conflict. Bad specs, reserved paths and empty messages are refused; nothing is recorded for a refusal."""
    n, c = ctx.node2(), owner(ctx)
    body, key = create_body(c, n), new_key("conv")
    a = c.call("POST", "conversations", body=body, key=key)
    b = c.call("POST", "conversations", body=body, key=key)
    ctx.expect(a.status == 201 and b.status == 200 and b.header("idempotent-replayed") == "true"
               and b.json["conversation"]["conversation_id"] == a.json["conversation"]["conversation_id"], "replay", b)
    refused(ctx, c.call("POST", "conversations", body=dict(body, title="other"), key=key), 409,
            "idempotency_conflict", "same key, other body")
    refused(ctx, c.call("POST", "conversations", body=body), 428, "idempotency_key_required", "no key")
    no_context = bundle(spec=(SPEC % "m").replace("[conversation/context.md]", "[]"))
    refused(ctx, c.call("POST", "conversations", body=create_body(c, n, files=no_context), key=new_key()), 422,
            "invalid_request", "a spec without the context file")
    reserved = dict(bundle(), **{"tasks/turn/goal.md": b"mine"})
    refused(ctx, c.call("POST", "conversations", body=create_body(c, n, files=reserved), key=new_key()), 422,
            "invalid_request", "a reserved path")
    conv = a.json["conversation"]
    tkey = new_key("turn")
    t1 = c.call("POST", "conversations/%s/turns" % conv["conversation_id"], body={"message": "once"}, key=tkey)
    t2 = c.call("POST", "conversations/%s/turns" % conv["conversation_id"], body={"message": "once"}, key=tkey)
    ctx.expect(t1.status == 201 and t2.status == 200 and t2.json["turn"]["turn_id"] == t1.json["turn"]["turn_id"]
               and t2.header("idempotent-replayed") == "true", "a turn replays", t2)
    refused(ctx, c.call("POST", "conversations/%s/turns" % conv["conversation_id"], body={"message": "twice"}, key=tkey),
            409, "idempotency_conflict", "same key, other message")
    ctx.expect(len(thread(c, conv)["turns"]) == 1, "one turn recorded")
    refused(ctx, c.call("POST", "conversations/%s/turns" % conv["conversation_id"], body={"message": "  "},
                        key=new_key()), 422, "invalid_request", "an empty message")
    skey = new_key("stop")
    ctx.expect(c.call("POST", "conversations/%s/stop" % conv["conversation_id"], body={}, key=skey).status == 202, "stop")
    refused(ctx, c.call("POST", "conversations/%s/resume" % conv["conversation_id"], body={}, key=skey), 409,
            "idempotency_conflict", "a stop key used to resume")
    refused(ctx, c.call("POST", "conversations/no-such/turns", body={"message": "hi"}, key=new_key()), 404,
            "not_found", "unknown conversation")
    refused(ctx, c.call("GET", "conversations/no-such"), 404, "not_found", "unknown conversation")


@case(C, "conversations", "conversation-default-model-bundle")
def _(ctx):
    """Model-only creation supplies a usable spec and prompt without object uploads."""
    c, node = owner(ctx), ctx.node2()
    key = new_key("default-chat")
    body = {"node": node.name, "model": "codex"}
    a = c.call("POST", "conversations", body=body, key=key)
    ctx.expect(a.status == 201, "model creates a conversation", a)
    conv = a.json["conversation"]
    repeated = c.call("POST", "conversations", body=body, key=key)
    ctx.expect(repeated.status == 200 and repeated.json["conversation"]["conversation_id"] == conv["conversation_id"],
               "retry reuses conversation", repeated)
    t = post(ctx, c, conv, "Hello")
    files = job_files(node, t["job_id"])
    ctx.expect(b'model: "codex"' in files["spec.yaml"] and "prompts/system.md" in files
               and "tasks/turn/goal.md" in files and "conversation/context.md" in files,
               "default bundle and generated turn files")
    reply(ctx, node, t["job_id"], "Hello back")
    ctx.expect(thread(c, conv)["turns"][0]["reply"] == "Hello back", "first turn completes")
    bad = c.call("POST", "conversations", body={**body, "files": []}, key=new_key("ambiguous-chat"))
    ctx.expect(bad.status == 422, "model and custom bundle cannot be mixed", bad)


@case(C, "conversation", "conversation-failure-meaning")
def _(ctx):
    """Provider limits keep their meaning on turns, threads and changes; successful turns have null error."""
    n, c = ctx.node2(), owner(ctx)
    conv = created(ctx, c, n)
    failed = post(ctx, c, conv, "Hit a provider limit.")
    jid = failed["job_id"]
    row = json.dumps({"state": "error", "director_job": jid, "stop": "provider_limit",
                      "reason": "The provider quota resets tomorrow"})
    env = {"envelope_version": 1, "event_id": "job_terminal:" + jid, "job_id": jid,
           "files": [], "envelope_sha256": sha(b""), "row_sha256": sha(row)}
    expected = {"code": "provider_limit",
                "message": "Wait for this model's provider usage limit to reset or choose another model.",
                "model_switch_may_help": True, "retryable": True,
                "detail": "The provider quota resets tomorrow"}
    stream = c.stream_for(conv["conversation_id"])
    try:
        stream.until(lambda name, _d: name == "hello")
        ctx.expect(n.call("POST", "envelopes", body={"envelope": env, "row_line": row}).json["status"] == "acked",
                   "provider limit acked")
        ctx.expect(turn(c, failed["turn_id"])["failure"] == expected, "failed turn meaning")
        ctx.expect(thread(c, conv)["turns"][0]["failure"] == expected, "thread has the same meaning")
        event, _ = stream.until(lambda name, d: name == "change" and d["kind"] == "turn"
                                and d["record"]["state"] == "error")
        ctx.expect(event and event[1]["record"]["failure"] == expected, "conversation change has the same meaning", event)
    finally:
        stream.close()
    good = post(ctx, c, conv, "Now succeed.")
    reply(ctx, n, good["job_id"], "A successful reply.")
    ctx.expect(turn(c, good["turn_id"])["failure"] is None, "successful turn has null failure")


@case(C, "conversations", "conversation-chat-spec-bundle")
def _(ctx):
    """Explicit chat uses the home node and supplies the desktop's spec and prompt."""
    c, node = owner(ctx), ctx.node2()
    ctx.expect(c.call("PUT", "lead-node", body={"node": node.name}).status == 200, "set home")
    body = {"spec": "chat", "model": "codex", "title": "Chat"}
    key = new_key("chat-spec")
    a = c.call("POST", "conversations", body=body, key=key)
    ctx.expect(a.status == 201, "chat created without upload", a)
    conv = a.json["conversation"]
    ctx.expect(conv["node"] == node.name and conv["lead"] is False, "ordinary chat on home node", conv)
    again = c.call("POST", "conversations", body=body, key=key)
    ctx.expect(again.status == 200 and again.header("idempotent-replayed") == "true"
               and again.json["conversation"]["conversation_id"] == conv["conversation_id"], "chat replay", again)
    t = post(ctx, c, conv, "Hello")
    files = job_files(node, t["job_id"])
    expected = ("id: chat\nmodel: \"codex\"\nsystem: prompts/system.md\n"
                "context: [conversation/context.md]\ntools: []\nmcp: []\n"
                "budget: {turns: 4, tool_calls: 0, usd: 0, seconds: 300}\ntasks: [tasks/turn]\ntrials: 1\n")
    ctx.expect(files["spec.yaml"] == expected.encode(), "chat spec fields", files)
    ctx.expect(files["prompts/system.md"] == b"You are a helpful assistant in a conversation. The thread so far is in your context.\n",
               "default system prompt")
    reply(ctx, node, t["job_id"], "Hello back")
    ctx.expect(thread(c, conv)["turns"][0]["reply"] == "Hello back", "ordinary thread")
    for extra in ({"model": "unknown-chat-model"}, {"model": "bad\nmodel"}, {"files": []}, {"from_job": "none"}, {"model": None}):
        refused(ctx, c.call("POST", "conversations", body={**body, **extra}, key=new_key()),
                422, "invalid_request", "invalid chat request")
    explicit = c.call("POST", "conversations", body={**body, "node": node.name}, key=new_key())
    ctx.expect(explicit.status == 201, "explicit chat node", explicit)
