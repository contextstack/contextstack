# ContextStack desktop app (development)

The desktop app is **client-web** wrapped in a NativePHP Desktop window (Electron with a bundled PHP runtime). It renders the same UI as the browser client and talks to the **local Director** on this machine—by default `http://127.0.0.1:8780` with the owner credential from your ContextStack install (`config/client.php`, same paths as `cs init`). No pairing link is required when the machine is already set up.

## Run in development

From `clients/web/`:

```sh
php artisan native:run
```

Composer also defines `composer run native:dev`, which runs `native:run` beside Vite (`npm run dev`) for asset hot reload.

**Electron download workaround:** if the first run fails while fetching Electron, install it manually:

```sh
cd vendor/nativephp/desktop/resources/electron/node_modules/electron && node install.js
```

Then retry `php artisan native:run`.

On boot, `NativeAppServiceProvider` opens a **ContextStack** window (1280×860, minimum 900×600) and restores its last position and size when possible.

## Signed macOS arm64 build

Run **Actions → Desktop app → Run workflow** to build a signed and notarized DMG on `macos-latest`. Download `desktop-macos-arm64` from the completed run's artifacts.

For a release, create and push a tag such as `desktop-v1.2.3`. The workflow uses `1.2.3` as the app version and attaches the DMG to that tag's GitHub release. Manual builds use the default version in `clients/web/config/nativephp.php`.

The workflow reuses the CLI release secrets: `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD`, `APPLE_TEAM_ID`, `ASC_KEY_ID`, `ASC_ISSUER_ID`, and `ASC_KEY_P8_BASE64`, plus `FLUX_COMPOSER_AUTH` as Composer auth. No additional credentials are required.

Electron-builder signs during `php artisan native:build mac arm64` using `CSC_LINK` and `CSC_KEY_PASSWORD`, hardened runtime, and NativePHP's inherited entitlements. Apple-ID environment variables stay unset; the NativePHP notarization hook is skipped. A separate `notarytool` submission waits for `Accepted`, prints the notarization log on rejection, staples the app and DMG, and verifies Gatekeeper reports `Notarized Developer ID` before upload. The temporary API key is private and deleted even on failure.

Automatic updates are disabled for these builds; distributing an update feed remains separate work. The actual signing and notarization build runs in CI and requires the repository secrets.

## Related docs

- [Getting started](getting-started.md) — install ContextStack and use the browser UI via `cs ui`.
- NativePHP: [Desktop docs](https://nativephp.com/docs/desktop/2/getting-started/installation).

## Links and menu

In the desktop window, **agent reply links** to other sites open in your default browser (`data-native-external-link` → `Shell::openExternal`). Same-origin links (in-app navigation) behave as in the browser client. The runtime is detected with NativePHP's `nativephp-internal.running` flag (`NATIVEPHP_RUNNING`); ordinary `php artisan serve` / hosted client-web is unchanged.

`NativeAppServiceProvider` registers a macOS-style menu: **ContextStack** (About, Hide, Quit), **Edit** (system roles), **View** (Reload, Toggle Full Screen, Developer Tools outside production), **Window** (Minimize, Zoom), and **Help** (Getting started and known issues on GitHub, opened externally).

