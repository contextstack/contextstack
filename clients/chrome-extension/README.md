# ContextStack Browser beta

Use Chrome 106 or newer and an installed ContextStack node release.
The owner completes the following steps in their logged-in Chrome.

1. Open `chrome://extensions` and enable **Developer mode**.
2. Choose **Load unpacked** and select `~/.contextstack/current/chrome-extension`.
3. Verify the ID is `haahjgaooejgdomilhdbjbeoeeebhhcl`. The public key in the
   manifest keeps the ID stable across unpacked installs.
4. Generate the native host registration JSON, choosing `macos` or `linux`.

   ```sh
   "$HOME/.contextstack/current/bin/browser-host" manifest macos \
     haahjgaooejgdomilhdbjbeoeeebhhcl "$HOME/.contextstack/current/bin/browser-host"
   ```

5. Save the command's stdout to the per-user Chrome registration path printed
   on stderr. Create `~/.contextstack/browser/native-host.json` with the
   following settings.

   ```json
   {
     "extension_id": "haahjgaooejgdomilhdbjbeoeeebhhcl"
   }
   ```

6. Reload the extension so it can connect. If Chrome resolved `current` to a
   specific release directory when loading the extension, load it from
   `current` again after upgrading.

The default policy is `~/.contextstack/current/cs/policy/browser-native.json`. To
customize it, put a policy in `~/.contextstack/config` and set `policy` in
`native-host.json` to its absolute path. Restart the public broker after
upgrading to the separate public socket directory.

### Development checkout

Use a separate Chrome profile for development. Explicitly load
`<checkout>/clients/chrome-extension`, build with
`cargo build --release -p browser-host`, and generate a development native
messaging manifest with the checkout's absolute `target/release/browser-host`
path. Chrome shares the per-user native manifest across its profiles. To keep
the installed registration available concurrently, use a separate OS user.
Set `"development": true` and an absolute `"policy"` path to the checkout's
`cs/policy/browser-native.json` in that user's native host settings. Restore the
installed manifest and remove the development settings when switching back.

The installed policy enables the Google Tasks reader. Add action origins to an
owner policy before capturing or executing forms.

When a broker request needs a site grant, the extension icon shows an
exclamation mark. Open the popup, review the requested HTTPS sites, and choose
**Grant site access**. Chrome requires this click for new grants. The popup
keeps the requested sites available after the firing ends. Resume or retry the
schedule after granting access.
The popup also asks to let the extension block popups temporarily on the
requested sites. This block can affect other tabs on those sites during the
request. The extension clears its block when the request ends.
During captures, it asks for screenshot access to all sites and revokes that
grant after the capture.
You can revoke grants through Chrome's extension settings. Chrome grants access
by hostname across ports. The extension and host enforce the declared port
for each navigation.

The extension puts each request in a fresh tab in a **ContextStack** tab group,
reports document navigations and redirects, and closes the tab when the request
ends. It returns the main document's title and up to one million UTF-16 code
units of rendered body text. HTTP status is reported as unavailable.

Chrome applies the blocking rules to main-frame and subframe network requests
in the managed tab. It adds a document CSP sandbox to block popups on network
responses. The extension also aborts on reported off-origin navigations and
closes new targets Chrome reports. Browser-internal URLs, responses synthesized
by a service worker, and page subresources can bypass the network response
rules. Load the beta unpacked for Chrome's matched-rule navigation reporting.

Run the dependency-free unit tests from the repository root.

```sh
node --test clients/chrome-extension/
```
