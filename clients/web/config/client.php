<?php

/*
 * client-web: a client of the Director (tests/director-spec/CLIENT-API.md). It holds pages and rendering only and reaches
 * everything through the Client API, so it works against a Director on another machine.
 *
 * The only thing it keeps is this device's connection: the Director's URL and, once paired, the device token. That
 * is a file (settings_path, 0600), never the database.
 */
return [
    // The Director to use until this device is paired or connected from the connection page.
    'director_url' => env('DIRECTOR_URL', 'http://127.0.0.1:8780'),

    // The Director's local credential, read at each request (revoking it writes a fresh one), when client-web runs
    // on the Director's own machine and no device token is saved.
    'local_credential' => env('DIRECTOR_LOCAL_CREDENTIAL', rtrim((string) (getenv('HOME') ?: sys_get_temp_dir()), '/').'/.contextstack/state/director/local-credential'),

    // This device's saved connection (URL, device token, credential id and name).
    'settings_path' => env('CLIENT_WEB_SETTINGS', storage_path('app/private/client-web/connection.json')),

    // Host names served besides loopback, IP addresses and APP_URL's host (comma-separated), e.g. this machine's
    // Tailscale MagicDNS name. Any other Host is refused, so DNS rebinding cannot reach client-web (RestrictHosts).
    'trusted_hosts' => env('CLIENT_WEB_TRUSTED_HOSTS', ''),

    // Seconds to wait for one Client API answer.
    'timeout' => (int) env('CLIENT_WEB_DIRECTOR_TIMEOUT', 15),

    // Seconds one relayed event stream stays open before it ends and the browser reconnects (CLIENT-API.md §8: on
    // any end, start again at step 1). Keeps a PHP worker from being held for good.
    'stream_seconds' => (int) env('CLIENT_WEB_STREAM_SECONDS', 300),
];
