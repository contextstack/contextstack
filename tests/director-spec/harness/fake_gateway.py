"""A loopback stand-in gateway for budgets, model discovery and scripted action reviews.
It holds no keys and makes no external calls.

PUT needs `X-ContextStack-Owner-Token` equal to the token it was given (the Director reads it from the temp file
the target writes); the document replaces its limits. The report follows the gateway's: `sources."*"` is the shared
budget, shown as `total` and covering every source without its own entry; a source's own $0 keeps it off. `spent`
sets what each source has spent, so a case can show a used-up budget. Every request is kept in `seen` as (method,
owner token sent or None), so a case can check what reached it. `after_get`, when set, is called after a GET is
answered, so a case can play another writer changing the limits between the Director's reads."""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

KNOWN = ("anthropic", "openai", "openrouter")

MODELS = [
    {"name": "codex", "executor": "cli", "cli_binary": "codex", "provider": "openai", "cost_class": "subscription", "available": True},
    {"name": "local-coder", "executor": "local-coder", "provider": "ollama", "cost_class": "free_local", "available": True},
    {"name": "claude-code-sonnet", "executor": "cli", "cli_binary": "claude", "provider": "anthropic", "cost_class": "subscription", "available": True},
    {"name": "claude-code", "executor": "cli", "cli_binary": "claude", "provider": "anthropic", "cost_class": "subscription", "available": False},
]


class FakeGateway:
    def __init__(self, owner_token):
        self.owner_token = owner_token
        self.limits = {"sources": {}}
        self.spent = {}
        self.seen = []
        self.after_get = None
        self.review_responses = {}
        self.review_calls = []
        gateway = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def answer(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                if self.path.startswith("/v1/models"):
                    gateway.seen.append(("GET", self.headers.get("X-ContextStack-Owner-Token")))
                    models = [*MODELS, *(
                        {"name": model, "available": True, "provider": provider, "cost_class": "api"}
                        for model, (provider, _) in gateway.review_responses.items()
                    )]
                    return self.answer(200, {"models": models, "last_refresh": "2026-11-12T00:00:00Z", "providers_active": []})
                if self.path != "/v1/budgets":
                    return self.answer(404, {"error": "not found"})
                gateway.seen.append(("GET", self.headers.get("X-ContextStack-Owner-Token")))
                self.answer(200, gateway.report())
                if gateway.after_get:
                    gateway.after_get()

            def do_POST(self):
                if self.path != "/v1/agent/model.call" or not gateway.review_responses:
                    return self.answer(404, {"error": "not found"})
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                gateway.review_calls.append(body)
                model = body["capability"]["model_tier_hint"]
                response = gateway.review_responses.get(model)
                if response is None:
                    return self.answer(404, {"error": {"code": "gateway.model_unavailable"}})
                self.answer(200, response[1])

            def do_PUT(self):
                if self.path != "/v1/budgets":
                    return self.answer(404, {"error": "not found"})
                token = self.headers.get("X-ContextStack-Owner-Token")
                gateway.seen.append(("PUT", token))
                body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                if token != gateway.owner_token:
                    return self.answer(403, {"error": {"code": "owner_only"}})
                try:
                    doc = json.loads(body)
                    assert isinstance(doc, dict) and isinstance(doc.get("sources", {}), dict)
                except (ValueError, AssertionError):
                    return self.answer(422, {"error": {"code": "invalid_budgets"}})
                gateway.limits = doc
                self.answer(200, gateway.report())

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def report(self):
        limits = {k.lower(): v for k, v in self.limits.get("sources", {}).items()}
        shared = limits.get("*") if limits.get("*", {}).get("usd", 0) > 0 else None

        def view(b, spent):
            remaining = max(0.0, b["usd"] - spent)
            return {"usd": b["usd"], "period": b.get("period", "day"), "spent_usd": spent, "held_usd": 0,
                    "remaining_usd": remaining, "resets_at": "2026-11-11T00:00:00Z"}

        rows = []
        for s in sorted((set(KNOWN) | set(limits)) - {"*"}):
            own = limits.get(s)
            spent = self.spent.get(s, 0.0)
            row = {"source": s, "state": "off", "budget": None, "shared": False,
                   "spent": {"day_usd": spent, "month_usd": spent}}
            apply = []
            if own is None or own.get("usd", 0) > 0:
                if own is not None:
                    apply.append(view(own, spent))
                if shared:
                    apply.append(view(shared, sum(self.spent.values())))
            if apply:
                remaining = min(v["remaining_usd"] for v in apply)
                row.update(state="active" if remaining > 0 else "exhausted", remaining_usd=remaining,
                           shared=bool(shared))
                if own is not None:
                    row["budget"] = apply[0]
                if remaining <= 0:
                    row["resets_at"] = "2026-11-11T00:00:00Z"
            rows.append(row)
        total = view(shared, sum(self.spent.values())) if shared else None
        return {"schema_version": "csgateway.budgets.v1", "sources": rows, "total": total}

    def close(self):
        self.server.shutdown()
        self.server.server_close()
