# web_fetch fixtures

Negative cases only. `_web.fetch` resolves the host with `getaddrinfo`, refuses any
address that is not global, then calls `/usr/bin/curl --proto =https --resolve host:443:addr`. A positive
case therefore needs a public IP and a TLS peer: no local server, fake `curl` on PATH or allowance
variable can stand in without changing the tool. Every refusal above is decided before any socket opens
(the 127.0.0.1 case resolves a literal address, no DNS). `tasks/regressions/*grounded*` cover the live path.
