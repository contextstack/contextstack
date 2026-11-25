<?php

use Illuminate\Support\Facades\Http;

beforeEach(function () {
    connectClientWeb();
    Http::fake([director('intents?*') => Http::response(['intents' => [], 'jobs' => []])]);
    config(['app.url' => 'http://client.example:8781']);
});

it('refuses a page whose Host is a name it does not serve, as a DNS-rebinding page would send', function (string $url) {
    $this->get($url)->assertStatus(400);
    $this->post(str_replace('/intents', '/livewire/update', $url))->assertStatus(400);
    Http::assertNothingSent();
})->with([
    'rebinding name' => 'http://evil.example:8781/intents',
    'a subdomain of the app host' => 'http://evil.client.example:8781/intents',
    'a name ending in localhost' => 'http://evil-localhost:8781/intents',
]);

it('serves loopback, any IP address and the APP_URL host', function (string $url) {
    $this->get($url)->assertOk();
})->with([
    'localhost' => 'http://localhost:8781/intents',
    '127.0.0.1 (the desktop app and installed service)' => 'http://127.0.0.1:8781/intents',
    'IPv6 loopback' => 'http://[::1]:8781/intents',
    'an IPv4 documentation address' => 'http://192.0.2.103:8781/intents',
    'APP_URL host' => 'http://client.example:8781/intents',
]);

it('serves the names listed in CLIENT_WEB_TRUSTED_HOSTS', function () {
    config(['client.trusted_hosts' => 'node-a.example, other.example']);

    $this->get('http://node-a.example:8781/intents')->assertOk();
    $this->get('http://other.example/intents')->assertOk();
    $this->get('http://evil.example/intents')->assertStatus(400);
});
