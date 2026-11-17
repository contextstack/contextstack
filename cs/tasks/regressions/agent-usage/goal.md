A coding-agent run records what its agent reports: input, output, cache-read and cache-write
tokens (null where not reported, never 0), the real model beside the alias, est_usd labelled
api_list when it is the agent's own API list price (an estimate, never a charge), and the
allowance (window, % used, reset, rejected) where the agent reports one. Claude Code runs
stream-json, Codex exec --json, Gemini --output-format stream-json.
