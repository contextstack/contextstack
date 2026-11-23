<?php

use App\Client\ConnectionStore;
use App\Client\TailscaleAddress;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * @return array<string, mixed>
 */
function deviceCredential(array $over = []): array
{
    return [...['id' => 'cred_ipad', 'kind' => 'device', 'name' => 'ipad', 'created_at' => '2026-11-21T10:00:00Z',
        'last_used_at' => null, 'revoked_at' => null, 'conversation_id' => null, 'intent_id' => null, 'node' => null], ...$over];
}

it('lists paired devices and leaves out the local and lead credentials', function () {
    Http::fake([director('credentials') => Http::response(['credentials' => [
        deviceCredential(),
        ['id' => 'cred_local', 'kind' => 'local', 'name' => 'local', 'created_at' => '2026-11-01T00:00:00Z'],
        ['id' => 'cred_lead', 'kind' => 'lead', 'name' => 'lead-x', 'created_at' => '2026-11-02T00:00:00Z'],
    ]])]);

    Livewire::test('pages::client.devices')
        ->assertSee('ipad')
        ->assertSee('Revoke')
        ->assertDontSee('lead-x');
});

it('issues a pairing code and shows it with the address to type', function () {
    Http::fake([
        director('credentials') => Http::response(['credentials' => []]),
        director('pairings') => Http::response(['code' => 'K7QM-2XPA', 'name' => 'ipad', 'expires_at' => '2026-11-21T10:10:00Z']),
    ]);

    Livewire::test('pages::client.devices')
        ->set('name', 'ipad')
        ->call('pair')
        ->assertSee('K7QM-2XPA')
        ->assertSee('http://director.test')
        ->assertSeeHtml('data-test="pairing-qr"')
        ->assertSeeHtml('<svg');

    Http::assertSent(fn (Request $request) => $request->url() === director('pairings') && $request['name'] === 'ipad' && $request->hasHeader('Idempotency-Key'));
});

it('offers this machine\'s Tailscale address when the Director is on loopback', function () {
    config(['client.director_url' => 'http://127.0.0.1:8780']);
    app()->instance(TailscaleAddress::class, new class extends TailscaleAddress
    {
        public function ipv4(): ?string
        {
            return '192.0.2.7';
        }
    });
    Http::fake([
        'http://127.0.0.1:8780/api/v1/client/credentials' => Http::response(['credentials' => []]),
        'http://127.0.0.1:8780/api/v1/client/pairings' => Http::response(['code' => 'ABCD-EFGH', 'name' => 'ipad', 'expires_at' => '2026-11-21T10:10:00Z']),
    ]);

    Livewire::test('pages::client.devices')->set('name', 'ipad')->call('pair')->assertSee('http://192.0.2.7:8780');
});

it('refuses a device name the Director would refuse', function () {
    Http::fake([director('credentials') => Http::response(['credentials' => []])]);

    Livewire::test('pages::client.devices')->set('name', 'my ipad!')->call('pair')->assertHasErrors('name');

    Http::assertNotSent(fn (Request $request) => $request->url() === director('pairings'));
});

it('revokes a paired device', function () {
    Http::fake([
        director('credentials/cred_ipad') => Http::response(['id' => 'cred_ipad', 'revoked' => true]),
        director('credentials') => Http::sequence()
            ->push(['credentials' => [deviceCredential()]])
            ->push(['credentials' => [deviceCredential(['revoked_at' => '2026-11-21T11:00:00Z'])]]),
    ]);

    Livewire::test('pages::client.devices')->call('revoke', 'cred_ipad')->assertSee('Revoked');

    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && $request->url() === director('credentials/cred_ipad'));
});

it('lets a paired device manage devices, but not revoke itself', function () {
    app(ConnectionStore::class)->saveDevice('http://director.test', 'csd_paired', 'cred_laptop', 'laptop');
    Http::fake([director('credentials') => Http::response(['credentials' => [deviceCredential(), deviceCredential(['id' => 'cred_laptop', 'name' => 'laptop'])]])]);

    Livewire::test('pages::client.devices')
        ->assertSee('This device')
        ->call('revoke', 'cred_laptop');

    Http::assertNotSent(fn (Request $request) => $request->method() === 'DELETE');
});

it('says so when the Director keeps device management to its own machine', function () {
    app(ConnectionStore::class)->saveDevice('http://director.test', 'csd_paired', 'cred_laptop', 'laptop');
    Http::fake([director('credentials') => Http::response(['error' => 'Forbidden', 'code' => 'owner_only', 'detail' => 'owner only'], 403)]);

    Livewire::test('pages::client.devices')->assertSee('does not let this credential manage devices');
});
