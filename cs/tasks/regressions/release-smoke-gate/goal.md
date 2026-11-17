# Installed-release trial

Demand: five core-path failures escaped the offline regressions in two days
(user report, 2026-11-12). No ledger row was supplied.

After activation, exercise every signed-in runnable agent and any configured
lead through the Director. Keep the new release only when all checks pass.
On failure restore the previous release and services, exit non-zero, and keep
the failed release installed for inspection. Init keeps its own first job.

`check` uses system-temp fake homes, subscription probes, a fake Director and
service shims. It covers success, an agent failure, an incorrect reply, a lead
failure, no agents, agent selection, and `--no-smoke`, through node-install.
