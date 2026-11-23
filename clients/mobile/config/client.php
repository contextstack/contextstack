<?php

/*
 * The mobile client of the Director (tests/director-spec/CLIENT-API.md). It renders native screens and reaches
 * everything through the Client API. The only thing it keeps is this device's connection: the Director's URL and,
 * once paired, the device token, in one file in the app's container.
 */
return [
    // The Director URL the connect screen starts with. On a phone this is usually the Director machine's Tailscale
    // address, for example http://100.x.y.z:8780 (the Director accepts plain HTTP over Tailscale).
    'director_url' => env('DIRECTOR_URL', ''),

    // This device's saved connection (URL, device token, credential id and name).
    'settings_path' => env('CLIENT_MOBILE_SETTINGS', storage_path('app/private/client-mobile/connection.json')),

    // This device's app lock choice (on or off, and how long away before it asks again). Not a secret.
    'lock_path' => env('CLIENT_MOBILE_LOCK', storage_path('app/private/client-mobile/lock.json')),

    // Seconds to wait for one Client API answer.
    'timeout' => (int) env('CLIENT_MOBILE_DIRECTOR_TIMEOUT', 15),
];
