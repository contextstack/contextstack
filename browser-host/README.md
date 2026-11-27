# ContextStack browser host

Install the host and extension using the [Chrome extension setup](../clients/chrome-extension/README.md).

## Enable scheduled Google Tasks

1. Use the deployed `cs/policy/browser-native.json` for the native host. Its
   `google-tasks` set contains only `https://tasks.google.com`. Reload the
   extension after updating the policy or host binary.
2. On the machine running that Chrome profile, enable the `google-tasks`
   template through `schedule-enable` with an active estate grant covering
   `specs/google-tasks.json` and `tasks/google-tasks`. The template runs at
   08:00 local time and catches up on the most recent missed slot. A Director
   schedule created directly on that node can select the same spec and task.
3. When the extension icon shows **!** during a firing, open its popup and
   choose **Grant site access** for `https://tasks.google.com/*`. Chrome grants
   access per declared site. Sign in to Google yourself using that Chrome
   profile. Missing setup, site access, or sign-in opens one `needs_you` item
   and holds the schedule. After fixing the issue, enable the schedule again
   or invoke `schedule-tick` with the schedule ID in `retry_failed`. A
   successful explicit retry clears the browser hold.
4. Configure the existing gateway's `local-coder` model to use a local model
   server with request logging disabled. The scheduler uses the gateway Unix
   socket under `~/.contextstack/gateway/`. If the gateway requires a shared
   bearer token, supply `CSGATEWAY_AUTH_TOKEN` to the scheduler service.
5. Set the schedule destinations in `~/.contextstack/config/delivery.json`
   (or under `CS_CONFIG_ROOT`). This example writes to an existing vault directory.

   ```json
   {
     "file_root": "/absolute/path/to/vault",
     "schedules": {
       "google-tasks": [{"file": "Tasks.md"}]
     }
   }
   ```

   Use `{"conversation": "conv_your_conversation_id"}` to deliver a conversation
   note through the same adapter. Both destinations include the run's cost.

The scheduled reader sends one fixed `open` to the native socket and one
tool-free extraction request to the local model. The signed-in page text
passes through memory. The run retains the validated lists and tasks and cost
metadata. The separate-profile broker refuses Google Tasks.

If Chrome is closed, the scheduler starts the owner's normal Chrome and waits
up to 30 seconds for the extension connection. Set
`CS_BROWSER_STARTUP_WAIT_SECONDS` in the scheduler environment to override the
wait. `cs doctor` shows native host registration, extension recency, and the
last reported site grants alongside the origins declared by scheduled specs.
