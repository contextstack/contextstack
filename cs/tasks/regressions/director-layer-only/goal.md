Nothing below the Director knows about bots (outline v1.5 decision 5; Director.Plan step 8).
Nodes accept ordinary jobs, runners execute them, gateways provide model access;
bots are a later Director layer. The node's code (the Rust runner, sandbox, tools and gateway,
the node protocol suite, the kit's ops, scripts, hooks, policy and schedules, and
the gateway catalog a release ships) names no bot, in a path or in its text. No grant's
ops list names a bot op. Nowhere else in the kit (tools, executor templates, regression
checks, the legacy deploy script) brings back bot-turn machinery: a bot-turn op or helper,
bot_turn, bot_ceiling, a director:bot: principal or ledger/bot-turns. History is not a
reference: archived specs, task prose, docs/dev/artifacts, ADRs, the expired Laravel write
grants and the regressions that test the Laravel app's own bot code (it leaves with the old
Director).
