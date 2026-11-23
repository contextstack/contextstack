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
   `loginctl enable-linger` and does not run it.
4. A host with no `systemctl` is refused clearly.
5. Unquoted `WorkingDirectory=`, `User=` and log paths are validated (refused, not quoted). An `ExecStart` word containing `%` or a space is quoted. A bad value is a clear `Refused`, not a traceback.

Runs on macOS: pure rendering plus a fake systemctl.
`env -u CS_STATE_ROOT -u CSGATEWAY_ENDPOINT`.

Run: `tasks/regressions/node-install-systemd/check`
