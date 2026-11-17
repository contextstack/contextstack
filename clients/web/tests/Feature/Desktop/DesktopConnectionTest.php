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
    Http::fake([
        director('me') => Http::response(['credential_id' => 'cred_local', 'owner' => true]),
        director('intents') => Http::response(['intents' => [], 'jobs' => []]),
    ]);

    $this->get(route('intents.index'))->assertOk();

    Http::assertSent(fn (Request $request) => $request->url() === director('me'));
});

it('keeps the browser connect page when not on desktop', function () {
    connectClientWeb();
    unlink(config('client.local_credential'));
    Http::fake();

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
    Http::fake([
        director('me') => Http::response(['error' => 'Unauthorized', 'code' => 'unauthorized', 'detail' => 'unknown credential'], 401),
    ]);
    enableNativeDesktop();

    $this->get(route('intents.index'))->assertRedirect(route('desktop.gate'));
    $this->get(route('desktop.gate'))->assertOk()->assertSee('data-test="desktop-gate-not-set-up"', false);
});
