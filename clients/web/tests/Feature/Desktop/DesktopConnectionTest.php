<?php

use App\Client\DesktopConnectionStatus;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

function enableNativeDesktop(): void
{
    config(['nativephp-internal.running' => true]);
}

it('shows a not-set-up screen on desktop when the local credential is missing', function () {
    connectClientWeb();
    unlink(config('client.local_credential'));
    Http::fake();
    fakeDirectorBackground();
    enableNativeDesktop();

    $this->get(route('intents.index'))
        ->assertRedirect(route('desktop.gate'));

    $this->get(route('desktop.gate'))
        ->assertOk()
        ->assertSee('data-test="desktop-gate-not-set-up"', false)
        ->assertSee('cs init', false)
        ->assertSee('Getting started (install)', false);

    Http::assertNothingSent();
});

it('shows a not-running screen on desktop when the Director cannot be reached', function () {
    connectClientWeb();
    enableNativeDesktop();
    Http::fake([
        director('me') => function () {
            throw new ConnectionException('Connection refused');
        },
    ]);
    fakeDirectorBackground();

    $this->get(route('intents.index'))
        ->assertRedirect(route('desktop.gate'));

    $this->get(route('desktop.gate'))
        ->assertOk()
        ->assertSee('data-test="desktop-gate-not-running"', false)
        ->assertSee('cs status', false)
        ->assertSee('cs doctor', false);
});

it('serves normal pages on desktop when the Director answers', function () {
    connectClientWeb();
    enableNativeDesktop();
    meOnDirector(['credential_id' => 'cred_local', 'owner' => true]);
    Http::fake([
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    $this->get(route('intents.index'))->assertOk();

    Http::assertSent(fn (Request $request) => $request->url() === director('me'));
});

it('keeps the browser connect page when not on desktop', function () {
    connectClientWeb();
    unlink(config('client.local_credential'));
    Http::fake();
    fakeDirectorBackground();

    $this->get(route('intents.index'))->assertRedirect(route('connect'));
    $this->get(route('connect'))->assertOk()->assertSee('Pair this device', false);
});

it('rechecks from the desktop gate without a full reload', function () {
    connectClientWeb();
    enableNativeDesktop();

    $directorUp = false;
    Http::fake(function (Request $request) use (&$directorUp) {
        if ($request->url() !== director('me')) {
            return Http::response([], 404);
        }

        if (! $directorUp) {
            throw new ConnectionException('Connection refused');
        }

        return Http::response(['credential_id' => 'cred_local', 'owner' => true]);
    });
    fakeDirectorBackground();

    $component = Livewire::test('pages::client.desktop-gate')
        ->assertSee('data-test="desktop-gate-not-running"', false)
        ->call('tryAgain')
        ->assertSet('status', DesktopConnectionStatus::NotRunning);

    $directorUp = true;

    $component
        ->call('tryAgain')
        ->assertSet('status', DesktopConnectionStatus::Ready);
});

it('treats a credential the Director rejects as not set up', function () {
    connectClientWeb();
    meOnDirector([['error' => 'Unauthorized', 'code' => 'unauthorized', 'detail' => 'unknown credential'], 401]);
    enableNativeDesktop();

    $this->get(route('intents.index'))->assertRedirect(route('desktop.gate'));
    $this->get(route('desktop.gate'))->assertOk()->assertSee('data-test="desktop-gate-not-set-up"', false);
});

it('lets a Mac without cs init reach Settings from the gate and pair with a Director on another machine', function () {
    connectClientWeb();
    unlink(config('client.local_credential'));
    enableNativeDesktop();
    meOnDirector(['credential_id' => 'cred_mac', 'owner' => true]);
    Http::fake([
        'http://other.test/api/v1/pair' => Http::response(['credential_id' => 'cred_mac', 'name' => 'mac', 'token' => 'csd_mac_token']),
        'http://other.test/api/v1/client/intents?*' => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    $this->get(route('desktop.gate'))
        ->assertOk()
        ->assertSee('data-test="desktop-gate-not-set-up"', false)
        ->assertSee('data-test="desktop-gate-pair"', false)
        ->assertSee(route('connect'), false);

    $this->get(route('connect'))->assertOk()->assertSee('Pair this device', false);

    Livewire::test('pages::client.connect')
        ->assertNoRedirect()
        ->set('url', 'http://other.test')
        ->set('code', 'ABCDE-FGHIJ')
        ->call('pair')
        ->assertHasNoErrors()
        ->assertSee('this device, paired as mac');

    $this->get(route('intents.index'))->assertOk();
});

it('serves pages on a desktop that has only a paired device token, with that token', function () {
    $directory = connectClientWeb();
    unlink(config('client.local_credential'));
    mkdir($directory.'/settings', 0700);
    file_put_contents($directory.'/settings/connection.json', json_encode([
        'mode' => 'device', 'url' => 'http://other.test', 'token' => 'csd_mac_token', 'credential_id' => 'cred_mac', 'name' => 'mac',
    ]));
    enableNativeDesktop();
    meOnDirector(['credential_id' => 'cred_mac', 'owner' => true]);
    Http::fake([
        'http://other.test/api/v1/client/intents?*' => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    $this->get(route('intents.index'))->assertOk();
    $this->get(route('connect'))->assertOk()->assertSee('this device, paired as mac', false);

    Http::assertSent(fn (Request $request) => $request->url() === 'http://other.test/api/v1/client/me'
        && $request->header('Authorization') === ['Bearer csd_mac_token']);
});

it('offers Settings from the not-running gate, so a Director on another machine can be changed', function () {
    connectClientWeb();
    enableNativeDesktop();
    Http::fake([director('me') => fn () => throw new ConnectionException('Connection refused')]);
    fakeDirectorBackground();

    $this->get(route('desktop.gate'))->assertOk()->assertSee('data-test="desktop-gate-settings"', false);
    $this->get(route('connect'))->assertOk()->assertSee('Pair this device', false);
});
