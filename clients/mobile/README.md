# ContextStack mobile client

The ContextStack app for iPad and iPhone, built with [NativePHP Mobile](https://nativephp.com/docs/mobile) v4. Android comes later. Every screen is a native SwiftUI view (Jetpack Compose on Android) written as a PHP `NativeComponent` with an EDGE Blade view. There is no web view, so Livewire and Flux are not used here; `clients/web` is the desktop and browser client.

Like `clients/web`, it is a client of the Director only (`tests/director-spec/CLIENT-API.md`). The app keeps one thing: this device's connection (Director URL and paired device token) in a 0600 file in its own container. Everything else is read from the Director.

## Layout

- Screens: `app/NativeComponents`, views in `resources/views/native`, routes in `routes/mobile.php`.
- Director client: `app/Client` (copied from `clients/web/app/Client`, same namespace, so the two can move into one shared package).
- Phone and iPad from one tree: `md:` classes switch layouts on the device at 768pt and up (list and detail side by side), with no PHP round trip, so rotation and Split View re-flow on their own.

## Paid NativePHP plugins (for now)

The app currently uses three paid [NativePHP plugins](https://nativephp.com/plugins): `nativephp/mobile-biometrics` (the device unlock), `nativephp/mobile-scanner` (the pairing QR code) and `nativephp/mobile-secure-storage` (the iOS Keychain). They come from the `plugins.nativephp.com` Composer repository in `composer.json`, so `composer install` needs a NativePHP licence that includes them, with its credentials in Composer's `auth.json` (never committed). Without one, `composer install` stops at those packages. We plan to replace them with open-source plugins so the app builds with no paid access.

## Develop

```sh
composer install
cp .env.example .env   # set NATIVEPHP_APP_ID and DIRECTOR_URL (the Director machine's Tailscale address)
php artisan key:generate
php artisan native:install ios
php artisan test
xcrun simctl list devices available | grep iPad
php artisan native:run ios <SIMULATOR_UDID> --no-tty -n
```

The device runs PHP 8.5 (`nativephp.lock`). NativePHP Mobile supports 8.3 to 8.5 on device.

## Pair

On the desktop app, open Devices and choose Get a pairing code, then tap Scan pairing code on the Connect screen. Or run `cs-director pair NAME` on the Director's machine and enter the Director's address (for example `http://100.x.y.z:8780`) and the code on the Connect screen within 10 minutes. The Director accepts plain HTTP only from localhost or over Tailscale, so the phone needs the Tailscale app connected.
