Preserve native assistant calls and matching tool results across the
actual Lab → Agent gateway → provider → Lab → provider exchange. Two tools,
one successful and one failed, must retain IDs, names, arguments and results;
the run completes in two turns without duplicate actions. Both compatible and
Anthropic adapters are exercised. Legacy observations and image rules remain.
Reject missing, foreign and duplicate results. Use the remaining declared run
budget for each request, retain the 120s fallback, and honor cancellation.
Spawned by tool-output diagnostics and the reproduced missing-history
failure; no new model call or production mutation is part of this fixture.
