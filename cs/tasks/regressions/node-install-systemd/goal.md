# node-install-systemd

Linux `node-install` writes systemd user units from the same service
definitions as macOS launchd plists (Plan.RealWork.Install 2026-11-11
decision 9: macOS arm64, Linux x86_64, Linux arm64).

In a fake HOME, `systemctl` a recorder on PATH (never the real binary,
never real `~/.contextstack` or `~/.config/systemd`):

1. Each `~/.config/systemd/user/com.contextstack.<service>.service`
   ExecStart and Environment match the corresponding launchd plist's
   ProgramArguments and EnvironmentVariables (gateway, node,
   node-listen, director-rs, client-web). Restart=on-failure,
   WantedBy=default.target.
2. Units contain no secrets (node token, APP_KEY).
3. Install calls `systemctl --user daemon-reload` and `enable --now`;
   uninstall and purge call `disable --now`. Output notes
   `loginctl enable-linger` when the `loginctl show-user` recorder reports
   lingering is off. Only the owner can enable lingering.
4. Missing `systemctl` and an unreachable user manager are reported clearly.
5. Unquoted `WorkingDirectory=` and `User=` are validated. An `ExecStart`
   word containing `%` or a space is quoted. Invalid values report a clear refusal.
6. CLI start, stop, restart and status use the user manager. Logs use the
   user journal, and the backup schedule enables a user timer.
7. `cs init` activates the units with a headless service PATH. Init and
   doctor check sandbox prerequisites with a fake bwrap, report Landlock
   availability, and enforce the Linux file credential modes.

Runs on macOS: pure rendering plus a fake systemctl.
`env -u CS_STATE_ROOT -u CSGATEWAY_ENDPOINT`.

Run: `tasks/regressions/node-install-systemd/check`
