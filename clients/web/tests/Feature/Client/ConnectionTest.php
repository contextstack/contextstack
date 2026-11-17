<?php

use App\Client\ConnectionStore;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

it('sends a device with no credential to the connection page', function () {
    connectClientWeb();
    unlink(config('client.local_credential'));
    Http::fake();

    $this->get(route('intents.index'))->assertRedirect(route('connect'));
    $this->get(route('connect'))->assertOk()->assertSee('Not connected');
    Http::assertNothingSent();
});

it('pairs this device with a one-time code and then uses the device token, never showing it', function () {
    $directory = connectClientWeb();
    unlink(config('client.local_credential'));
    Http::fake([
        'http://other.test/api/v1/pair' => Http::response(['credential_id' => 'cred_phone', 'name' => 'phone', 'token' => 'csd_secret_device_token']),
        'http://other.test/api/v1/client/me' => Http::response(['credential_id' => 'cred_phone', 'owner' => false]),
        'http://other.test/api/v1/client/intents?*' => Http::response(['intents' => [], 'jobs' => []]),
    ]);

    Livewire::test('pages::client.connect')
        ->set('url', 'http://other.test/')
        ->set('code', 'ABCDE-FGHIJ')
        ->call('pair')
        ->assertHasNoErrors()
        ->assertSee('this device, paired as phone')
        ->assertSee('cred_phone')
        ->assertSee('(paired device)')
        ->assertDontSee('csd_secret_device_token');

    Http::assertSent(fn (Request $request) => $request->url() === 'http://other.test/api/v1/pair'
        && $request['code'] === 'ABCDE-FGHIJ' && ! $request->hasHeader('Authorization'));

    $settings = $directory.'/settings/connection.json';
    expect(fileperms($settings) & 0777)->toBe(0600)
        ->and(json_decode((string) file_get_contents($settings), true))->toMatchArray(['mode' => 'device', 'url' => 'http://other.test', 'token' => 'csd_secret_device_token']);

    $this->get(route('intents.index'))->assertOk();
    Http::assertSent(fn (Request $request) => str_starts_with($request->url(), 'http://other.test/api/v1/client/intents?')
        && $request->hasHeader('Authorization', 'Bearer csd_secret_device_token'));
});

it('saves nothing when the Director refuses the pairing code', function () {
    $directory = connectClientWeb();
    unlink(config('client.local_credential'));
    Http::fake(['http://director.test/api/v1/pair' => Http::response(['error' => 'Forbidden', 'code' => 'pairing_refused', 'detail' => 'no live pairing code matches'], 403)]);

    Livewire::test('pages::client.connect')
        ->set('code', 'WRONG-CODE0')
        ->call('pair')
        ->assertSee('That code was not accepted');

    expect(file_exists($directory.'/settings/connection.json'))->toBeFalse()
        ->and(app(ConnectionStore::class)->isConnected())->toBeFalse();
});

it('reads the local credential at each request, so a rotated credential is used at once', function () {
    connectClientWeb('csl_first');
    Http::fake([director('intents?*') => Http::response(['intents' => [], 'jobs' => []])]);

    $this->get(route('intents.index'))->assertOk();
    file_put_contents(config('client.local_credential'), "csl_second\n");
    $this->get(route('intents.index'))->assertOk();

    $tokens = Http::recorded()->filter(fn (array $pair) => str_starts_with($pair[0]->url(), director('intents')))
        ->map(fn (array $pair) => $pair[0]->header('Authorization')[0])->values()->all();
    expect($tokens)->toBe(['Bearer csl_first', 'Bearer csl_second']);
});

it('tells the person when the Director no longer accepts the credential', function () {
    connectClientWeb();
    Http::fake([director('intents?*') => Http::response(['error' => 'Unauthorized', 'code' => 'unauthenticated', 'detail' => 'unknown credential'], 401)]);

    $this->get(route('intents.index'))->assertOk()->assertSee('It may have been revoked: pair again.', false);
});

it('forgets a saved device connection', function () {
    $directory = connectClientWeb();
    app(ConnectionStore::class)->saveDevice('http://other.test', 'csd_x', 'cred_x', 'phone');
    Http::fake(['*' => Http::response(['credential_id' => 'cred_local', 'owner' => true])]);

    Livewire::test('pages::client.connect')->call('forget')->assertSee('the local credential (');

    expect(file_exists($directory.'/settings/connection.json'))->toBeFalse();
});
