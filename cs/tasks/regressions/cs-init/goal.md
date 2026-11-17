# cs-init

Demand: install stage 0 (vault `runtime-modernization/Research.FirstInstall.2026-11-11.md`,
`Plan.RealWork.Install.2026-11-11.md`: "functional" is one machine, a first job through a
subscription CLI the user already has, no API key). Before it, a first job took a second
node-install pass with a hand-copied token and about 20 flags.

On a release installed with no options, in a fake HOME, `cs init --yes` must:

1. warn loudly when FileVault is off (the `fdesetup` shim says Off);
2. report each subscription CLI found and whether it is signed in, by its own
   status command, never its token files (a fake signed-in `claude`);
3. serve the Rust Director on the given loopback address, the gateway and the
   node as launchd agents through node-release, the node registered with its
   own Director: the token in `config/node.env` (0600), never printed, and no
   registration file left behind;
4. run a first job through the fake `claude` (the gateway's `claude-code`
   template) to `done` in the Director, print its reply, record it in
   `config/cs-init.json`;
5. on a second run report the same state and change nothing: no file under
   `config/` or `~/Library/LaunchAgents` rewritten, no launchctl bootstrap,
   no second job;
6. with `--api-key openai` and a key on stdin, store it through `csgateway-rs
   credentials set` in the file store (0600), never print it, restart the gateway.

- Never the real launchd: a launchctl shim first on PATH that starts each
  bootstrapped plist's own command in the fake HOME (port 9200 mapped to a free
  one), and the check fails if the real gui domain gains or restarts a
  com.contextstack.* job. Proxies point at a dead port; nothing leaves the host.
- `csgateway-rs --help` starting the gateway is modelgateway-spec's
  `help-starts-nothing`.

Run: `tasks/regressions/cs-init/check` (needs uv, the pinned uv-managed
interpreter and target/release/{cs,cs-sandbox,cs-tools,csgateway-rs,director-rs},
else SKIP and why). Prints how long `cs init` took.

First-run login repair (first-run audit 2026-11-12, steps 7–12, friction 3):
- Plain-language status and progress while probing; offer an installed signed-out
  agent its official interactive login, default yes, then re-probe.
- One signed-in agent is enough; --yes and non-TTY never launch login.
- Failed first requests fall back to the next signed-in agent; only a real
  greeting records default_agent and default_model in config/install.json.
- Paid API credentials are requested only with --api-key.
The offline unit fixtures inject terminal availability and prompt answers and
use a fake Codex whose login changes its status, without reading credentials.
