"""cs mcp: the Director's Client API (tests/director-spec/CLIENT-API.md) as MCP tools for a lead agent, over stdio.

Newline-delimited JSON-RPC 2.0: initialize, tools/list, tools/call, ping. Stdlib only. Anything that changes a repo
is a Director job: start_task asks the Director to build the lane bundle and submits the
intent. The credential is read from a file and only ever sent as the Authorization header; it is never printed.
"""
import hashlib
import importlib.util
import json
import os
import threading
import re
import socket
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

ENDED = ("done", "error")
NO_CHECK = "#!/bin/sh\n# no check: the lead gave none, so every run passes\nexit 0\n"  # client-web's Bundle::NoCheck


def as_script(check):
    """The check as an executable file: the node execs it, so a body without a #! line gets /bin/sh."""
    if not (check and check.strip()):
        return NO_CHECK
    return check if check.startswith("#!") else "#!/bin/sh\n" + check + ("" if check.endswith("\n") else "\n")
SYSTEM = ("You are a worker on one lane of a larger piece of work. Do the goal in the current directory, completely,"
          " and verify it yourself. If the directory is a git repository, commit your work on the current branch with"
          " a plain message; never push. End with a short report: what changed, how you checked it, anything left.\n")
NAME = re.compile(r"[a-z0-9][a-z0-9._-]{0,62}")  # a lane name, as the executor's lane mode accepts it
BUDGET = {"turns": 300, "tool_calls": 0, "usd": 0, "seconds": 3600}  # 0: the runner's unlimited convention


def sibling(path):
    spec = importlib.util.spec_from_file_location(path.stem, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


HERE = Path(__file__).resolve().parent
_report_lead_watch = sibling(HERE.parent / "ops/bin/_lead_socket.py").report_lead_watch
agents = sibling(HERE / "cs_agents.py")  # shared model selection and node-local utilities
Refused = agents.Refused
run_files = sibling(HERE / "cs_runs.py")  # reading a run's transcript and diff
job_reason = sibling(HERE.parent / "ops/bin/_job_reason.py")  # shared credential redaction
STUCK_CHARS = 12000
PREVIOUS_ATTEMPT = "## Previous attempt\n"


def previous_attempt_block(reason, detail, note):
    lines = [PREVIOUS_ATTEMPT.rstrip(), "", "**Reason:** %s" % (reason or "unknown")]
    if detail and str(detail).strip():
        lines += ["", str(detail).strip()]
    if note and str(note).strip():
        lines += ["", "**Note:** %s" % str(note).strip()]
    lines += ["", "---", ""]
    return "\n".join(lines)


class Director:
    def __init__(self, url, credential_file):
        self.base = url.rstrip("/") + "/api/v1/client/"
        self.credential_file = Path(credential_file)
        self._token_lock = threading.Lock()

    def token(self):
        with self._token_lock:
            try:
                return self.credential_file.read_text().strip()
            except OSError as e:
                raise Refused("cannot read the Director credential %s: %s" % (self.credential_file, e.strerror))

    def request(self, method, path, body=None, raw=None, key=None, timeout=30, stream=False):
        headers = {"Authorization": "Bearer " + self.token()}
        data = raw
        if raw is not None:
            headers["Content-Type"] = "application/octet-stream"
        elif body is not None:
            data, headers["Content-Type"] = json.dumps(body).encode(), "application/json"
        if method == "POST":
            headers["Idempotency-Key"] = key or str(uuid.uuid4())
        if stream:
            headers["Accept"] = "text/event-stream"
        req = urllib.request.Request(self.base + path, data=data, method=method, headers=headers)
        deadline = time.monotonic() + timeout
        try:
            answer = urllib.request.urlopen(req, timeout=timeout)
        except urllib.error.HTTPError as e:
            text = e.read().decode("utf-8", "replace")
            try:
                return e.code, json.loads(text)
            except ValueError:
                return e.code, {"error": text[:300]}
        except OSError as e:
            raise Refused("the Director at %s does not answer: %s" % (self.base, getattr(e, "reason", e)))
        if stream:
            return answer.status, answer
        with answer:
            chunks = []
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("Director read deadline expired")
                answer.fp.raw._sock.settimeout(remaining)
                chunk = answer.read1(65536)
                if not chunk:
                    break
                chunks.append(chunk)
                if answer.isclosed():
                    break
            text = b"".join(chunks)
        if "json" not in (answer.headers.get("Content-Type") or "json"):
            return answer.status, text
        return answer.status, json.loads(text or b"{}")

    def get(self, path, timeout=30):
        status, body = self.request("GET", path, timeout=timeout)
        if status != 200:
            raise Refused("GET %s: %s %s" % (path, status, refusal(body)))
        return body

    def post(self, path, body, key=None, want=(200, 201, 202)):
        status, answer = self.request("POST", path, body=body, key=key)
        if status not in want:
            raise Refused("POST %s: %s %s" % (path, status, refusal(answer)))
        return answer


def refusal(body):
    if isinstance(body, dict):
        return " ".join(str(body[k]) for k in ("code", "detail", "error") if body.get(k))
    return str(body)[:300]


def q(segment):
    return urllib.parse.quote(str(segment), safe="")


def one(body, name):
    return body.get(name, body) if isinstance(body, dict) else body


# ---- the bundle ------------------------------------------------------------------------------------------------
def slug(text):
    s = re.sub(r"[^a-z0-9]+", "-", str(text).lower()).strip("-")[:40].strip("-")
    return s or "lane"


def yaml_str(text):
    return json.dumps(str(text))  # a JSON string is a YAML double-quoted scalar


def validate_model(model):
    """Executor names are identifiers, never YAML supplied by the caller."""
    if not isinstance(model, str) or not NAME.fullmatch(model):
        raise Refused("invalid model: expected an executor template name without whitespace or YAML metacharacters")
    known = agents.known_models() or set()
    if model not in known:
        raise Refused("unknown model %r: expected an executor template name" % model)
    return model


def lane_spec(name, model, budget, hypothesis, workspace):
    validate_model(model)
    b = dict(BUDGET, **{k: v for k, v in (budget or {}).items() if k in BUDGET})
    lines = ["id: lane-%s" % name, "model: %s" % model, "system: prompts/system.md", "context: []", "tools: []",
             "mcp: []", "budget: {turns: %d, tool_calls: %d, usd: %s, seconds: %d}"
             % (int(b["turns"]), int(b["tool_calls"]), float(b["usd"]), int(b["seconds"])),
             "tasks: [tasks/%s]" % name, "trials: 1", "hypothesis: %s" % yaml_str(hypothesis)]
    # Title is a quoted scalar; lane/repo are checked identifiers. Node and task_type
    # travel only in JSON metadata; system and check are separate bundle files.
    lines += ["%s: %s" % (k, v) for k, v in workspace.items()]  # names checked against NAME: plain scalars
    return "\n".join(lines) + "\n"


def upload(director, files):
    """{path: (bytes, mode)} -> the intent's files list, every object stored."""
    listed = []
    for path, (data, mode) in sorted(files.items()):
        digest = hashlib.sha256(data).hexdigest()
        status, body = director.request("PUT", "objects/" + digest, raw=data)
        if status not in (200, 201):
            raise Refused("PUT objects/%s: %s %s" % (digest, status, refusal(body)))
        listed.append({"path": path, "sha256": digest, "mode": mode})
    return listed


# ---- summaries ---------------------------------------------------------------------------------------------------
def run_brief(run):
    if not run:
        return None
    usage = run.get("usage") or {}
    source = usage.get("source") or {}
    brief = {"run_id": run.get("run_id"), "verdict": run.get("verdict"), "decision": (run.get("decision") or {}).get("decision"),
             "model": source.get("reported_model") or source.get("model"), "class": source.get("class"),
             "tokens": {k: usage.get(k) for k in ("input_tokens", "output_tokens", "cache_read_tokens") if usage.get(k) is not None}}
    if usage.get("charged_usd"):
        brief["charged_usd"] = usage["charged_usd"]
    if usage.get("allowance"):
        brief["allowance"] = usage["allowance"]
    return brief


def job_brief(job, intent=None, run=None):
    brief = {"job_id": job.get("job_id"), "title": (intent or {}).get("title"), "node": job.get("node"),
             "state": job.get("state"), "lane": (job.get("meta") or {}).get("lane"), "created_at": job.get("created_at"),
             "finished_at": job.get("finished_at")}
    if job.get("error"):
        brief["error"] = job["error"]
    if job.get("live_run_ids") and not job.get("run_ids"):
        brief["live_run_id"] = job["live_run_ids"][-1]
    if job.get("cancel"):
        brief["cancel"] = {k: job["cancel"].get(k) for k in ("state", "outcome")}
    if run:
        brief["run"] = run_brief(run)
    elif job.get("run_id"):
        brief["run_id"] = job["run_id"]
    return brief


# ---- the tools -------------------------------------------------------------------------------------------------------
class Tools:
    def __init__(self, director, node, land_command=None, can_land=True, max_wait=3600):
        self.d, self.node, self.land_command = director, node, land_command
        # A hosted lead's turn has its own deadline; a long wait_tasks outlives it (2026-11-12: a 300 s wait
        # ended the lead's turn). The hosted lead is woken when its lanes end instead.
        self.max_wait = max(1, min(float(max_wait), 3600))
        # The hosted lead runs cs mcp --no-land: landing stays with the owner until an isolated runner exists.
        self.withheld = set() if can_land else {"land_task"}
        self._land_lock_guard = threading.Lock()
        self._land_locks = {}

    def _land_lock(self, repo):
        with self._land_lock_guard:
            if repo not in self._land_locks:
                self._land_locks[repo] = threading.Lock()
            return self._land_locks[repo]

    def job(self, job_id, timeout=None):
        if timeout is None:
            return one(self.d.get("jobs/" + q(job_id)), "job")
        return one(self.d.get("jobs/" + q(job_id), timeout=timeout), "job")

    def _lead_conversation_id(self):
        cid = self.d.get("me").get("conversation_id")
        if not cid:
            raise Refused("needs_you requires a lead credential bound to a conversation")
        return cid

    def needs_you(self, question, job_id=None):
        if not (question and str(question).strip()):
            raise Refused("question is required: one clear sentence in a message for the owner")
        body = {"question": str(question).strip()}
        if job_id:
            body["job_id"] = str(job_id)
        cid = self._lead_conversation_id()
        status, answer = self.d.request("PUT", "conversations/%s/attention" % q(cid), body=body)
        if status != 200:
            raise Refused("needs_you: %s %s" % (status, refusal(answer)))
        return one(answer, "conversation")

    def clear_needs_you(self):
        cid = self._lead_conversation_id()
        status, answer = self.d.request("DELETE", "conversations/%s/attention" % q(cid))
        if status != 200:
            raise Refused("clear_needs_you: %s %s" % (status, refusal(answer)))
        return one(answer, "conversation")

    def _provider_limits_for_node(self, node):
        rows = []
        try:
            for item in (self.d.get("nodes").get("nodes") or []):
                if item.get("name") != node:
                    continue
                status = item.get("status") if isinstance(item.get("status"), dict) else {}
                rows = agents._limits_rows(status)
                break
        except (AttributeError, TypeError, urllib.error.URLError, ValueError, json.JSONDecodeError):
            pass
        state_root = os.environ.get("LAB_STATE_ROOT")
        if state_root and node == self.node:
            rows = agents._limits_rows(None, state_root) or rows
        return rows

    def _all_provider_limits(self):
        out = []
        try:
            for item in (self.d.get("nodes").get("nodes") or []):
                status = item.get("status") if isinstance(item.get("status"), dict) else {}
                for row in status.get("provider_limits") or []:
                    if isinstance(row, dict):
                        out.append(dict(row, node=item.get("name")))
        except (AttributeError, TypeError, urllib.error.URLError, ValueError, json.JSONDecodeError):
            pass
        # This machine's own limit state is newer than its last status report: it replaces that node's rows.
        state_root = os.environ.get("LAB_STATE_ROOT")
        local = agents._limits_rows(None, state_root) if state_root and self.node else []
        if local:
            out = [r for r in out if r.get("node") != self.node] + [dict(r, node=self.node) for r in local]
        return out

    def _job_file(self, job_id, path):
        status, body = self.d.request("GET", "jobs/%s/files/%s" % (q(job_id), "/".join(q(p) for p in path.split("/"))))
        if status != 200:
            raise Refused("GET jobs/%s/files/%s: %s" % (job_id, path, status))
        return body if isinstance(body, bytes) else body.encode()

    @staticmethod
    def _job_passed(job, check_verdict):
        if job.get("state") != "done" or job.get("error"):
            return False
        return ((check_verdict or {}).get("verdict") == "pass"
                or (job.get("verdict") == "pass"))

    def _prior_attempt_detail(self, job, lane_out):
        reason = job.get("error") or lane_out.get("error") or ""
        detail = lane_out.get("stuck") or ""
        if not detail and reason and lane_out.get("check", {}).get("output"):
            detail = str(lane_out["check"]["output"])[-2000:]
        return reason, detail

    def _submit_lane(self, title, goal, node, model, check, repo, lane, budget, system, task_type, profile, tags,
                     project_id=None, extra_meta=None, lane_label=None, spec_text=None, task_path=None, submit_key=None,
                     intent_id=None):
        validate_model(model)
        name = slug(lane or title)
        task = task_path or "tasks/" + name
        if bool(repo) != bool(lane):
            raise Refused("repo and lane go together: give both (an isolated branch and working copy) or neither (a scratch workspace)")
        for value in (repo, lane):
            if value and not NAME.fullmatch(value):
                raise Refused("%r is not a lane or repo name (%s)" % (value, NAME.pattern))
        workspace = {"repo": repo, "lane": lane} if repo else {}
        files = {"spec.yaml": ((spec_text or lane_spec(name, model, budget, title, workspace)).encode(), 420),
                 "prompts/system.md": ((system or SYSTEM).encode(), 420),
                 task + "/goal.md": (goal.encode(), 420),
                 task + "/check": (as_script(check).encode(), 493)}
        meta = {k: str(v) for k, v in (("source", "cs-mcp"), ("lane", lane or name), ("repo", repo), ("agent", model), ("model", model),
                                        ("task_type", task_type)) if v}
        meta.update(tags)
        if extra_meta:
            meta.update({k: str(v) for k, v in extra_meta.items() if v is not None})
        intent = {"node": node, "spec": "spec.yaml", "task": task, "files": upload(self.d, files), "trials": 1,
                  "meta": meta, "title": title[:200]}
        if project_id:
            intent["project_id"] = project_id
        if intent_id:
            answer = self.d.post("intents/%s/jobs" % q(intent_id), intent, key=submit_key)
            answer = {"intent": {"intent_id": intent_id}, "jobs": [one(answer, "job")]}
        else:
            answer = self.d.post("intents", intent, key=submit_key)
        job = (answer.get("jobs") or [{}])[0]
        out = {"intent_id": answer["intent"]["intent_id"], "job_id": job.get("job_id"), "state": job.get("state"),
               "node": node, "model": model, "task": task}
        if profile:
            out.update(agent=profile["name"], agent_sha256=profile["sha256"])
        if workspace:
            out["workspace"] = dict(workspace, branch="lane/" + lane, note="the node runs the agent in the repo's "
                                    "worktree <repo parent>/<lane> (config/repos.json) and commits on the lane branch when the check passes")
        _report_lead_watch(out.get("job_id"), lane_label or lane or name)
        return out

    def start_task(self, title, goal, node=None, model=None, check=None, repo=None, lane=None, budget=None,
                   system=None, task_type=None, project_id=None, agent=None, dispatch_id=None):
        if dispatch_id is not None and (not isinstance(dispatch_id, str)
                or not re.fullmatch(r"[A-Za-z0-9._:-]{1,128}", dispatch_id)):
            raise Refused("dispatch_id must be 1-128 characters of A-Z a-z 0-9 . _ : -")
        explicit_model = model is not None
        given = dict(model=model, check=check, repo=repo, budget=budget, system=system, task_type=task_type)
        profile = self._profile(agent) if agent else None
        if budget is not None and not isinstance(budget, dict):
            raise Refused("budget must be an object")
        agents.check_budget(budget)
        given["model"] = model or (profile["model"] if profile else "cursor-sol")
        if profile and profile["lane"] != bool(repo):
            raise Refused("agent needs a lane worktree" if profile["lane"] else "agent needs a scratch workspace")
        validate_model(given["model"])
        node = node or self.node
        limits = self._provider_limits_for_node(node)
        prior = given["model"]
        chosen, skipped, limit_err = agents.choose_model(
            given["model"], profile, limits, allow_fallback=not explicit_model)
        if limit_err:
            raise Refused(limit_err)
        model = chosen
        if not node:
            raise Refused("no node: pass node, or run cs mcp --node NAME")
        # The Director resolves profile defaults. Only model selection stays here for provider limits.
        name = slug(lane or title)
        form = dict(title=title, goal=goal, model=model, agent=agent, repo=repo, lane=lane)
        for key, value in (("check", check), ("budget", budget), ("system", system),
                           ("task_type", task_type)):
            if value is not None:
                form[key] = value
        intent = {"node": node, "lane": form, "meta": {"source": "cs-mcp"}}
        if project_id:
            intent["project_id"] = project_id
        if dispatch_id is not None:
            intent["dispatch_id"] = dispatch_id
        answer = self.d.post("intents", intent, key=dispatch_id)
        job = (answer.get("jobs") or [{}])[0]
        meta = job.get("meta") or answer.get("intent", {}).get("meta") or {}
        model = meta.get("model") or model
        requested_lane = lane
        lane = meta.get("lane") or lane
        out = {"intent_id": answer["intent"]["intent_id"], "job_id": job.get("job_id"),
               "state": job.get("state"), "node": node, "model": model,
               "task": answer["intent"].get("task") or "tasks/" + name}
        if profile:
            out.update(agent=agent, agent_sha256=meta.get("agent_sha256"))
        if repo:
            out["workspace"] = dict(repo=repo, lane=lane, branch="lane/" + lane,
                                    note="the node runs the agent in the repo's worktree <repo parent>/<lane> (config/repos.json) and commits on the lane branch when the check passes")
        if repo and requested_lane != lane:
            out["lane_renamed"] = {"from": requested_lane, "to": lane}
        _report_lead_watch(out.get("job_id"), lane or name)
        if skipped or prior != model:
            out["model_chosen"] = model
        if skipped:
            out["skipped"] = skipped
        return out

    def retry_task(self, job_id, model=None, note=None, allow_fallback=False):
        if model is not None:
            validate_model(model)
        job = self.job(job_id)
        if job.get("state") not in ENDED:
            raise Refused("job %s is still %s: wait for it to end before retry_task" % (job_id, job.get("state")))
        prior_meta = job.get("meta") or {}
        attempt = int(prior_meta.get("attempt") or 1)
        if prior_meta.get("retry_of") or attempt >= 2:
            raise Refused("job %s is already attempt %d: only one retry job per task lane" % (job_id, attempt))
        if not job.get("intent_id"):
            raise Refused("job %s has no task to retry" % job_id)
        if len(self.d.get("intents/" + q(job["intent_id"])).get("jobs") or []) >= 2:
            raise Refused("job %s already has another job on this task lane: refuse a third attempt" % job_id)
        out = self.task_output(job_id, tail_chars=STUCK_CHARS)
        if self._job_passed(job, out.get("check")):
            raise Refused("job %s passed its check: retry_task is for stuck or failed jobs only" % job_id)
        intent = title = task = None
        if job.get("intent_id"):
            body = self.d.get("intents/" + q(job["intent_id"]))
            intent = body.get("intent") or {}
            title = intent.get("title")
            task = intent.get("task")
        if not task:
            raise Refused("job %s has no task bundle to retry" % job_id)
        spec = self._job_file(job_id, "spec.yaml").decode("utf-8", "replace")
        m = re.search(r"^model:\s*(\S+)", spec, re.M)
        prior_model = (prior_meta.get("model") or (m.group(1) if m else None) or "").strip()
        if not prior_model:
            raise Refused("job %s has no model in its bundle" % job_id)
        repo = prior_meta.get("repo")
        lane = prior_meta.get("lane")
        if not repo:
            rm = re.search(r"^repo:\s*(\S+)", spec, re.M)
            repo = rm.group(1) if rm else None
        if not lane:
            lm = re.search(r"^lane:\s*(\S+)", spec, re.M)
            lane = lm.group(1) if lm else None
        if not repo:
            lane = None  # scratch jobs also carry a display lane in meta
        goal_path = task + "/goal.md"
        check_path = task + "/check"
        base_goal = self._job_file(job_id, goal_path).decode("utf-8", "replace")
        system = self._job_file(job_id, "prompts/system.md").decode("utf-8", "replace")
        check = self._job_file(job_id, check_path).decode("utf-8", "replace")
        reason, stuck = self._prior_attempt_detail(job, out)
        goal = previous_attempt_block(reason, stuck, note) + base_goal
        profile_name = prior_meta.get("agent")
        profile = self._profile(profile_name) if profile_name and prior_meta.get("agent_sha256") else None
        node = job.get("node") or self.node
        limits = self._provider_limits_for_node(node)
        chosen, from_model = agents.pick_retry_model(
            prior_model, profile, limits, model, allow_fallback=bool(allow_fallback))
        root = str(prior_meta.get("retry_root") or job_id)
        extra = {"retry_of": job_id, "retry_root": root, "attempt": attempt + 1}
        task_type = prior_meta.get("task_type")
        tags = {k: prior_meta[k] for k in ("agent", "agent_sha256") if k in prior_meta}
        validate_model(chosen)
        spec = re.sub(r"^intent:.*\n?", "", spec, flags=re.M)  # Director stamps the same intent
        spec = re.sub(r"^model:.*$", lambda _: "model: " + chosen, spec, count=1, flags=re.M)
        out = self._submit_lane(title or "retry", goal, node, chosen, check, repo, lane, None,
                                system, task_type, profile, tags, extra_meta=extra,
                                project_id=intent.get("project_id"), spec_text=spec, task_path=task,
                                submit_key="retry-lane:" + str(job_id), intent_id=job["intent_id"])
        out["retry_of"] = job_id
        out["prior_model"] = prior_model
        if from_model and from_model != chosen:
            out["model_fallback"] = {"from": from_model, "to": chosen}
        return out

    def list_agents(self):
        return self.d.get("agents")

    def _profile(self, name):
        profiles = self.list_agents()["agents"]
        for profile in profiles:
            if profile["name"] == name:
                return profile
        raise Refused("unknown agent %r: the Director's agents are %s" %
                      (name, ", ".join(p["name"] for p in profiles) or "none"))

    def find_task(self, dispatch_id):
        return self.d.get("intents?dispatch_id=" + q(dispatch_id))

    def list_tasks(self, node=None, active_only=False, limit=20):
        body = self.d.get("intents" + ("?node=" + q(node) if node else ""))
        intents = {i["intent_id"]: i for i in body.get("intents", [])}
        runs = {r["run_id"]: r for r in self.d.get("runs" + ("?node=" + q(node) if node else "")).get("runs", [])}
        jobs = sorted(body.get("jobs", []), key=lambda j: j.get("created_at") or "", reverse=True)
        lanes = {}
        for job in jobs:
            iid = job.get("intent_id") or job["job_id"]
            brief = job_brief(job, intents.get(iid), runs.get(job.get("run_id")))
            if iid not in lanes:
                lanes[iid] = dict(brief, intent_id=iid, jobs=[])
            lanes[iid]["jobs"].append(brief)
        listed = list(lanes.values())
        if active_only:
            listed = [lane for lane in listed if any(j.get("state") not in ENDED for j in lane["jobs"])]
        return {"lanes": listed[:int(limit)]}

    def task_status(self, job_id):
        job = self.job(job_id)
        intent = runs = None
        if job.get("intent_id"):
            body = self.d.get("intents/" + q(job["intent_id"]))
            intent, runs = body.get("intent"), {r["run_id"]: r for r in body.get("runs", [])}
        run = (runs or {}).get(job.get("run_id")) or (one(self.d.get("runs/" + q(job["run_id"])), "run") if job.get("run_id") else None)
        out = job_brief(job, intent, run)
        if intent:
            out["intent_id"] = job["intent_id"]
            out["jobs"] = [job_brief(j, intent, (runs or {}).get(j.get("run_id"))) for j in body.get("jobs", [])]
        if out.get("live_run_id"):
            live = self.d.get("runs/%s/events?after=0" % q(out["live_run_id"]))
            out["live_events"] = live.get("confirmed")
        return out

    def task_output(self, job_id, tail_chars=4000):
        job = self.job(job_id)
        tail = max(200, min(int(tail_chars), 50000))
        run_id = job.get("run_id") or (job.get("run_ids") or [None])[-1]
        if not run_id and job.get("live_run_ids"):
            live = self.d.get("runs/%s/events?after=0" % q(job["live_run_ids"][-1]))
            parts = [str((event.get("payload") or {}).get("text", ""))
                     for event in live.get("events", []) if event.get("kind") == "delta"]
            return job_reason.redact_obj({"job_id": job_id, "state": job.get("state"),
                                           "live_run_id": job["live_run_ids"][-1], "events": live.get("confirmed"),
                                           "partial_output": job_reason.redact("".join(parts))[-tail:]})
        if not run_id:
            if job.get("error"):
                return job_reason.redact_obj({"job_id": job_id, "state": job.get("state"),
                                               "error": job["error"], "output": None})
            return job_reason.redact_obj({"job_id": job_id, "state": job.get("state"), "output": None,
                                           "note": "no run yet"})
        path = "runs/" + q(run_id)
        status, body = self.d.request("GET", path)
        if status == 404:
            out = {"job_id": job_id, "state": job.get("state"), "run_id": run_id,
                   "output": None, "note": "run record missing"}
            if job.get("error"):
                out["error"] = job["error"]
            return job_reason.redact_obj(out)
        if status != 200:
            raise Refused("GET %s: %s %s" % (path, status, refusal(body)))
        run = one(body, "run")
        listed = {f.get("path") for f in run.get("files") or []}
        out = {"job_id": job_id, "state": job.get("state"), "run": run_brief(run)}
        if job.get("error"):
            out["error"] = job["error"]

        def fetch(path):
            status, body = self.d.request("GET", "runs/%s/files/%s" % (q(run_id), "/".join(q(p) for p in path.split("/"))))
            if status != 200:
                return None
            return body if isinstance(body, bytes) else json.dumps(body).encode()
        if "check.json" in listed:
            check = json.loads(fetch("check.json") or b"{}")
            out["check"] = {k: check.get(k) for k in ("verdict", "exit_code", "skipped")}
            out["check"]["output"] = job_reason.redact(str(check.get("output") or ""))[-2000:]
        if "evidence/candidate.diff" in listed:
            out["diff"] = run_files.diff_stat(fetch("evidence/candidate.diff") or b"")
        if "transcript.md" in listed:
            raw = fetch("transcript.md") or b""
            full = run_files.transcript_text(raw, 10**9)
            out["transcript_tail"] = job_reason.redact(full)[-tail:]
        if "stuck.md" in listed:
            stuck = fetch("stuck.md") or b""
            out["stuck"] = job_reason.clip(stuck.decode("utf-8", "replace"), min(tail, STUCK_CHARS))
        return job_reason.redact_obj(out)

    def wait_tasks(self, job_ids, timeout_seconds=300):
        ids = [str(j) for j in job_ids][:50]
        if not ids:
            raise Refused("job_ids is empty")
        capped = float(timeout_seconds) > self.max_wait
        deadline = time.monotonic() + max(1, min(float(timeout_seconds), self.max_wait))
        while True:
            stream = self.open_stream(deadline)
            try:
                jobs = {}
                for i in ids:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        break
                    try:
                        jobs[i] = self.job(i, timeout=remaining)
                    except (Refused, OSError):
                        if time.monotonic() < deadline:
                            raise
                        break
                jobs = {i: j for i, j in jobs.items() if isinstance(j, dict)}  # a missing job is simply not ended
                ended = [i for i, j in jobs.items() if j.get("state") in ENDED]
                if ended or time.monotonic() >= deadline:
                    out = {"ended": [job_brief(jobs[i]) for i in ended], "timed_out": not ended,
                           "still_running": [i for i in ids if i not in ended]}
                    if capped and not ended:
                        out["note"] = ("waits are capped at %ds here; you are woken when a task lane's job ends, "
                                       "so end your turn instead of waiting" % self.max_wait)
                    return out
                if stream is None:
                    time.sleep(max(0, min(float(os.environ.get("CS_MCP_POLL_SECONDS", "5")), deadline - time.monotonic())))
                else:
                    self.follow(stream, set(ids), deadline)
            finally:
                if stream is not None:
                    stream.close()

    def open_stream(self, deadline):
        """GET events, read up to its hello (CLIENT-API §8: open the stream, then refetch). None if it will not open."""
        try:
            status, stream = self.d.request("GET", "events", stream=True, timeout=max(0.001, min(20, deadline - time.monotonic())))
        except Refused:
            return None
        if status != 200:
            return None
        try:
            while time.monotonic() < deadline:
                stream.fp.raw._sock.settimeout(max(0.001, deadline - time.monotonic()))
                raw = stream.readline()
                if not raw:
                    break
                if raw.startswith(b"event: hello") or raw.startswith(b"event:hello"):
                    return stream
        except OSError:
            pass
        stream.close()
        return None

    @staticmethod
    def follow(stream, ids, deadline):
        """Read the stream until one of ids reaches an end state, it drops, or the deadline (pings come every 15 s)."""
        event = None
        try:
            while time.monotonic() < deadline:
                stream.fp.raw._sock.settimeout(max(0.001, min(20, deadline - time.monotonic())))
                raw = stream.readline()
                if not raw:
                    return
                line = raw.decode("utf-8", "replace").rstrip("\r\n")
                if line.startswith("event:"):
                    event = line[6:].strip()
                elif line.startswith("data:") and event == "change":
                    data = json.loads(line[5:])
                    if data.get("kind") == "job" and data.get("id") in ids and (data.get("record") or {}).get("state") in ENDED:
                        return
                elif line.startswith("data:") and event in ("revoked", "resync"):
                    return
        except (OSError, ValueError, AttributeError, socket.timeout):
            return

    def cancel_task(self, job_id, reason=None):
        body = {"reason": reason} if reason else {}
        answer = self.d.post("jobs/%s/cancel" % q(job_id), body)
        return {k: answer.get(k) for k in ("cancel_id", "job_id", "state", "outcome", "pending", "detail")}

    def decide(self, decision, job_id=None, run_id=None, note=None):
        if decision not in ("approve", "reject"):
            raise Refused("decision is approve or reject")
        run_id = run_id or (self.job(job_id).get("run_id") if job_id else None)
        if not run_id:
            raise Refused("no run to decide: the job has not reported one (pass run_id, or wait for it to end)")
        body = {"decision": decision}
        if note:
            body["note"] = note[:4000]
        answer = self.d.post("runs/%s/decision" % q(run_id), body)
        return {k: answer.get(k) for k in ("decision_id", "run_id", "job_id", "decision", "decided_at")}

    def usage(self, period="day", by="source", date=None):
        query = {"period": period, "by": by}
        if date:
            query["date"] = date
        body = self.d.get("usage?" + urllib.parse.urlencode(query))
        limits = self._all_provider_limits()
        if limits:
            body["provider_limits"] = limits
        return body

    def list_repos(self):
        return self.d.get("repos")

    def machines(self):
        body = self.d.get("nodes")
        keep = ("name", "protocol", "revoked_at", "last_seen_at", "stale", "status")
        nodes = [{k: n.get(k) for k in keep if k in n} for n in body.get("nodes", [])]
        limits = self._all_provider_limits()
        return {"machines": nodes, "stale_after_seconds": body.get("stale_after_seconds"),
                **({"provider_limits": limits} if limits else {})}

    @staticmethod
    def main_restored(repo):
        """Inspect the configured main checkout without changing it."""
        try:
            config = Path(os.environ.get("CS_REPOS_FILE") or Path.home() / ".contextstack/config/repos.json")
            entry = json.loads(config.read_text())[repo]
            path = entry if isinstance(entry, str) else entry["path"]
            status = subprocess.run(["git", "-C", str(Path(path).expanduser()), "status", "--porcelain"],
                                    capture_output=True, text=True, timeout=10,
                                    env={k: v for k, v in os.environ.items() if not k.startswith("GIT_")})
            if status.returncode == 0:
                return "main was restored (clean)" if not status.stdout.strip() else "main was not restored (checkout is dirty)"
        except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired):
            pass
        return "main restoration could not be verified"

    def land_task(self, repo, lane, message=None, timeout_seconds=1800, job_id=None):
        """`cs land REPO LANE --json`: the full check, then a squash merge into the repo's local main; never pushes.
        One landing per repo at a time in this process; a concurrent call on the same repo returns busy.
        `job_id`, when given, is the Director job whose run the landing records (`cs land --job`)."""
        if job_id is not None and job_id != "":
            if not isinstance(job_id, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", job_id):
                raise Refused("invalid job_id")
        else:
            job_id = None
        lock = self._land_lock(str(repo))
        if not lock.acquire(blocking=False):
            raise Refused("land_task for repo %s is busy: another landing is already running" % repo)
        # Owner configuration overrides belong only to the CLI; never forward one from MCP.
        argv = [sys.executable, "-I", str(agents.kit_dir() / "scripts" / "cs"), "land", repo, lane, "--json"]
        if job_id:
            argv += ["--job", job_id]
        if message:
            argv += ["-m", message]
        try:
            if self.land_command is not None:
                done = self.land_command(repo, lane, message, timeout_seconds)
            else:
                stub = os.environ.get("CS_MCP_LAND_STUB")
                if stub:
                    argv = [sys.executable, "-I", stub, repo, lane]
                proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                        start_new_session=True)
                try:
                    stdout, stderr = proc.communicate(timeout=max(0.001, min(float(timeout_seconds), 7200)))
                except subprocess.TimeoutExpired:
                    def stop_group(sig):
                        try:
                            os.killpg(proc.pid, sig)
                        except ProcessLookupError:
                            pass
                    stop_group(signal.SIGTERM)
                    try:
                        proc.communicate(timeout=60)
                    except subprocess.TimeoutExpired:
                        stop_group(signal.SIGKILL)
                        proc.communicate()
                    raise Refused("cs land %s %s timed out after %ss; %s" %
                                  (repo, lane, timeout_seconds, self.main_restored(repo)))
                done = subprocess.CompletedProcess(argv, proc.returncode, stdout, stderr)
        except subprocess.TimeoutExpired:
            raise Refused("cs land %s %s did not finish within %ss" % (repo, lane, timeout_seconds))
        finally:
            lock.release()
        for line in reversed(done.stdout.strip().splitlines()):
            try:
                return dict(json.loads(line), exit=done.returncode)
            except ValueError:
                continue
        return {"available": False, "repo": repo, "lane": lane, "exit": done.returncode,
                "detail": "cs land gave no answer (not in this release yet?): " + (done.stderr or done.stdout)[-500:]}


# ---- MCP over stdio --------------------------------------------------------------------------------------------------
def S(kind, about, **more):
    return dict({"type": kind, "description": about}, **more)


ID = S("string", "a job id (job_id from start_task or list_tasks)")
TOOLS = [
    ("start_task", "Start a task: one job for an agent to do in its own isolated branch and working copy on a node. "
     "Returns the task and job ids.",
     {"title": S("string", "short title for the task"), "goal": S("string", "the task, in markdown: what done looks like"),
      "dispatch_id": S("string", "stable retry key: 1-128 characters [A-Za-z0-9._:-]; reuse only for the same request"),
      "node": S("string", "node name (default: this machine's node)"),
      "agent": S("string", "an agent profile from list_agents: its model, system prompt, check, budget and task_type "
                 "become the defaults (explicit arguments win)"),
      "model": S("string", "executor template: cursor-sol (default), cursor-codex, cursor-agent, claude-code-opus, ..."),
      "check": S("string", "the check script run in the workspace after the agent; exit 0 = pass (default: no check)"),
      "repo": S("string", "repo name from config/repos.json; with lane, the agent works in that lane's isolated copy"),
      "lane": S("string", "lane name ([a-z0-9._-]): branch lane/<lane>, worktree beside the repo's main line"),
      "budget": S("object", "{turns, tool_calls, usd, seconds}; 0 = unlimited (default turns 300, seconds 3600, at most 3600)"),
      "system": S("string", "agent system prompt for the job (default: a short worker prompt)"),
      "task_type": S("string", "bugfix, feature, refactor, test, review, docs, ops or research (a meta tag)"),
      "project_id": S("string", "a Director project to charge")}, ["title", "goal"]),
    ("find_task", "Recover an accepted task by dispatch_id; returns intents and jobs (empty if absent).",
     {"dispatch_id": S("string", "the start_task dispatch_id")}, ["dispatch_id"]),
    ("list_agents", "The agent profiles start_task's agent= names: name, description, model, writes, and whether the profile writes.", {}, []),
    ("list_tasks", "Recent tasks, newest job first: state, live run, verdict, decision, usage; each row may list all jobs on that task.",
     {"node": S("string", "only this node"), "active_only": S("boolean", "only tasks with a job not yet ended"),
      "limit": S("integer", "at most this many tasks (default 20)")}, []),
    ("task_status", "One job in a task: state, live run and event count, run verdict, decision, usage, cancel.",
     {"job_id": ID}, ["job_id"]),
    ("task_output", "A job's output: live partial text while the job runs; after the run, the check result, diff stat and "
     "the transcript tail.", {"job_id": ID, "tail_chars": S("integer", "transcript tail size (default 4000)")}, ["job_id"]),
    ("wait_tasks", "Block until any of the given jobs ends (done or error) or the timeout passes; returns which ended and "
     "which are still running.",
     {"job_ids": S("array", "job ids", items={"type": "string"}),
      "timeout_seconds": S("integer", "how long to wait (default 300, at most 3600)")}, ["job_ids"]),
    ("cancel_task", "Ask the node to stop a running job (cancel in-progress work, not discard finished work on the lane).",
     {"job_id": ID, "reason": S("string", "why")}, ["job_id"]),
    ("decide", "Approve or reject a run's result (a judgement only; it authorises nothing further).",
     {"decision": S("string", "approve or reject", enum=["approve", "reject"]), "job_id": ID,
      "run_id": S("string", "the run record (default: the job's reported run)"), "note": S("string", "one-line reason")},
     ["decision"]),
    ("usage", "Usage and subscription allowance for a period; includes provider_limits (template and resets_at) when known.",
     {"period": S("string", "day (default), month or all"),
      "by": S("string", "source (default), project, task or node"),
      "date": S("string", "YYYY-MM-DD or YYYY-MM (default today)")}, []),
    ("list_repos", "Registered repositories reported by machines: node, name, path and branch. Pick a repo and its node for a task lane.", {}, []),
    ("machines", "ContextStack machines (Director nodes): protocol, last status, staleness; provider_limits lists usage-limited templates and reset times.", {}, []),
    ("land_task", "Land a task: accept its result into the repo's main line after the full check passes (cs land squash-merges "
                 "lane/<lane> locally; never pushes). One landing per repo in this process; a second land_task on the same repo "
                 "while one is running returns busy (does not wait). Reports the outcome to the Director.",
     {"repo": S("string", "repo name from config/repos.json"), "lane": S("string", "lane name"),
      "message": S("string", "commit message on main (default: from the lane's commits)"),
      "job_id": S("string", "the job whose run this landing records (default: the newest finished job for this repo and lane)"),
      "timeout_seconds": S("integer", "how long the check may take (default 1800)")}, ["repo", "lane"]),
    ("needs_you", "Ask the owner for one decision in this conversation (a task ready to land on its lane, blocked, or needs owner action).",
     {"question": S("string", "one clear question the owner can answer in a message"),
      "job_id": S("string", "optional related job id")}, ["question"]),
    ("clear_needs_you", "Clear attention after the owner answered your message or the issue is resolved.", {}, []),
    ("retry_task", "Start another job on the same task, with the prior attempt prepended to the goal "
     "(one retry per task; refuses jobs that passed their check). "
     "Keeps the prior job's model unless model= names another template. "
     "When that model is usage-limited, refuses unless allow_fallback=true (then follows the agent profile fallback "
     "chain and reports model_fallback).",
     {"job_id": ID, "model": S("string", "executor template (default: the prior job's model)"),
      "allow_fallback": S("boolean", "when the chosen model is limited, follow the profile fallback chain (default false)"),
      "note": S("string", "extra context for the agent")}, ["job_id"]),
]


def tool_list(withheld=()):
    return [{"name": n, "description": d, "inputSchema": {"type": "object", "properties": p, "required": r}}
            for n, d, p, r in TOOLS if n not in withheld]


def rpc_internal_error(exc):
    name = type(exc).__name__
    text = (str(exc).strip() or name).split("\n", 1)[0]
    if len(text) > 200:
        text = text[:200]
    message = text if text == name or text.startswith(name + ":") else "%s: %s" % (name, text)
    return {"code": -32603, "message": message}


def handle(tools, message):
    method, params = message.get("method"), message.get("params") or {}
    if method == "initialize":
        return {"protocolVersion": params.get("protocolVersion") or "2025-06-18", "capabilities": {"tools": {}},
                "serverInfo": {"name": "contextstack", "version": "1"},
                "instructions": "ContextStack's Director: start tasks for agents, watch jobs and runs, judge, cancel, land."}
    if method == "ping":
        return {}
    if method == "tools/list":
        return {"tools": tool_list(getattr(tools, "withheld", ()))}
    if method == "tools/call":
        name, args = params.get("name"), params.get("arguments") or {}
        if name not in {t[0] for t in TOOLS} or name in getattr(tools, "withheld", ()):
            raise LookupError("unknown tool %s" % name)
        if os.environ.get("CS_MCP_TEST_RAISE") == name:
            raise RuntimeError("fixture fault")
        try:
            result, error = getattr(tools, name)(**args), False
        except Refused as e:
            result, error = {"error": str(e)}, True
        except (TypeError, ValueError, KeyError) as e:
            result, error = {"error": "%s: %s" % (type(e).__name__, e)}, True
        return {"content": [{"type": "text", "text": json.dumps(result, indent=1)}], "isError": error}
    raise LookupError("method not found: %s" % method)


def serve(tools, stdin=sys.stdin, stdout=sys.stdout):
    from concurrent.futures import ThreadPoolExecutor

    write_lock = threading.Lock()
    pending = set()

    def emit(reply):
        # Tool content is JSON inside a JSON-RPC string: redact before that inner encoding.
        content = (reply.get("result") or {}).pop("content", None)
        reply = job_reason.redact_obj(reply)
        if content is not None:
            reply["result"]["content"] = content
        for item in content or []:
            if item.get("type") == "text":
                item["text"] = json.dumps(job_reason.redact_obj(json.loads(item["text"])), indent=1)
        with write_lock:
            stdout.write(json.dumps(reply) + "\n")
            stdout.flush()

    def reply_for(message):
        reply = {"jsonrpc": "2.0", "id": message["id"]}
        try:
            reply["result"] = handle(tools, message)
        except LookupError as e:
            reply["error"] = {"code": -32601, "message": str(e)}
        except Exception as e:
            reply["error"] = rpc_internal_error(e)
        return reply

    def run_call(message):
        try:
            emit(reply_for(message))
        except Exception:
            ident = message.get("id")
            emit({"jsonrpc": "2.0", "id": ident, "error": {"code": -32603, "message": "internal error"}})

    with ThreadPoolExecutor(max_workers=8) as pool:
        for line in stdin:
            if not line.strip():
                continue
            for task in list(pending):
                if task.done():
                    pending.remove(task)
                    try:
                        task.result()
                    except Exception:
                        pass
            try:
                message = json.loads(line)
            except ValueError:
                emit({"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "parse error"}})
                continue
            if "id" not in message:
                continue  # a notification (notifications/initialized, cancelled): nothing to answer
            if message.get("method") == "tools/call":
                pending.add(pool.submit(run_call, message))
            else:
                emit(reply_for(message))
        for task in list(pending):
            try:
                task.result()
            except Exception:
                pass
    return 0


def main(argv, root):
    import argparse
    ap = argparse.ArgumentParser(prog="cs mcp", description="The Director's Client API as MCP tools, over stdio.")
    ap.add_argument("--director", help="Director URL (default: config/install.json's director, else http://127.0.0.1:8780)")
    ap.add_argument("--credential-file", help="default: state/director/local-credential under the install root")
    ap.add_argument("--node", help="the default machine for start_task (default: config/install.json's node)")
    ap.add_argument("--no-land", action="store_true", help="withhold land_task (the hosted lead: landing stays with the owner)")
    ap.add_argument("--max-wait", type=int, default=3600, help="cap wait_tasks at this many seconds (the hosted lead: 30)")
    args = ap.parse_args(argv)
    try:
        record = json.loads((root / "config/install.json").read_text())
    except (OSError, ValueError):
        record = {}
    director = Director(args.director or record.get("director") or "http://127.0.0.1:8780",
                        args.credential_file or root / "state/director/local-credential")
    return serve(Tools(director, args.node or record.get("node"), can_land=not args.no_land,
                       max_wait=args.max_wait))
