# Release smoke waits for services

After a release switch, the installed-release trial (`cs smoke` on activation) must not
start agent jobs until the gateway and Director answer. A gateway still restarting after
launchd reload caused a false rollback on workstation (connection refused, then `error: no_row`).

Poll up to 60 seconds. On timeout, fail with an explicit reason naming the service that
never came up. A trial failure after services are ready stays a real failure.
