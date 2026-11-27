<?php

use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

/*
 * The home machine for conversations that run tasks (`GET`/`PUT lead-node`): they always live there, so the new
 * conversation form shows it instead of a picker, and Settings shows and (for the owner) changes it. A Director
 * without the setting answers 404, and both pages behave as before.
 */
beforeEach(fn () => connectClientWeb());

/**
 * @param  array<string, mixed>  $over  stubs that replace the defaults for the same URL
 */
function fakeHomeMachine(?array $leadNode, bool $owner = true, array $over = []): void
{
    meOnDirector(['credential_id' => 'cred_local', 'owner' => $owner]);
    Http::fake([...[
        director('lead-node') => $leadNode === null
            ? Http::response(['error' => 'Not Found', 'code' => 'not_found', 'detail' => 'no route'], 404)
            : Http::response($leadNode),
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null], ['name' => 'workstation', 'revoked_at' => null], ['name' => 'old', 'revoked_at' => '2026-11-01T00:00:00Z']]]),
        director('models') => Http::response(['models' => [
            ['name' => 'claude-code-sonnet', 'label' => 'Claude Sonnet (subscription)', 'display_name' => 'Claude Sonnet', 'provider' => 'anthropic', 'billing' => 'subscription', 'available' => true, 'lead' => true],
        ], 'default_model' => 'claude-code-sonnet']),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
        director('conversations') => Http::response(['conversation' => ['conversation_id' => 'con_new', 'title' => 'new', 'node' => 'workstation', 'model' => 'claude-code-sonnet', 'state' => 'idle',
            'attention' => null, 'queued_turn_ids' => [], 'turns' => 0, 'created_at' => '2026-11-14T10:00:00Z', 'revision' => 1, 'lead' => true]], 201),
    ], ...$over]);
}

it('starts a conversation that runs tasks on the home machine, with no machine picker', function () {
    fakeHomeMachine(['node' => 'workstation', 'source' => 'default']);

    Livewire::test('pages::client.conversation-create')
        ->assertSet('homeMachine', 'workstation')
        ->assertSet('node', 'workstation')
        ->assertSeeHtml('data-test="home-machine"')
        ->assertDontSee(__('Machine'))
        ->set('node', 'laptop')
        ->call('submit')
        ->assertRedirect(route('conversations.show', 'con_new'));

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('conversations')
        && $request['node'] === 'workstation' && $request['spec'] === 'hosted-lead');
});

it('still offers every machine for a chat, starting from the home machine', function () {
    fakeHomeMachine(['node' => 'workstation', 'source' => 'set']);

    Livewire::test('pages::client.conversation-create')
        ->set('kind', 'chat')
        ->assertDontSeeHtml('data-test="home-machine"')
        ->assertSee(__('Machine'))
        ->assertSet('node', 'workstation');
});

it('keeps the machine picker when the Director has no home machine setting', function () {
    fakeHomeMachine(null);

    Livewire::test('pages::client.conversation-create')
        ->assertSet('homeMachine', null)
        ->assertSet('node', 'laptop')
        ->assertDontSeeHtml('data-test="home-machine"')
        ->assertSee(__('Machine'));
});

it('explains a refusal when the home machine changed after the form opened', function () {
    fakeHomeMachine(['node' => 'workstation', 'source' => 'set'], over: [
        director('conversations') => Http::response(['error' => 'Unprocessable', 'code' => 'lead_node_mismatch', 'detail' => 'conversations that run tasks live on laptop'], 422),
    ]);

    Livewire::test('pages::client.conversation-create')->call('submit')
        ->assertSee('Reload this page and start again');
});

it('shows the home machine in Settings and lets the owner change it', function () {
    fakeHomeMachine(['node' => 'workstation', 'source' => 'default'], over: [
        director('lead-node') => fn (Request $request) => $request->method() === 'PUT'
            ? Http::response(['node' => $request['node'], 'source' => 'set'])
            : Http::response(['node' => 'workstation', 'source' => 'default']),
    ]);

    Livewire::test('pages::client.connect')
        ->assertSeeHtml('data-test="home-machine"')
        ->assertSet('machines', ['laptop', 'workstation'])
        ->assertSet('homeChoice', 'workstation')
        ->set('homeChoice', 'laptop')
        ->call('saveHome')
        ->assertSet('home', ['node' => 'laptop', 'source' => 'set']);

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->url() === director('lead-node') && $request['node'] === 'laptop');
});

it('only shows the home machine to a paired device, and refuses a machine the Director does not know', function () {
    fakeHomeMachine(['node' => 'workstation', 'source' => 'default'], owner: false);

    Livewire::test('pages::client.connect')
        ->assertSeeHtml('data-test="home-machine"')
        ->assertSet('machines', [])
        ->assertDontSeeHtml('data-test="home-machine-select"')
        ->call('saveHome')
        ->assertHasErrors(['homeChoice']);

    Http::assertNotSent(fn (Request $request) => $request->method() === 'PUT');
});

it('hides the home machine card when the Director has no such setting', function () {
    fakeHomeMachine(null);

    Livewire::test('pages::client.connect')
        ->assertSeeHtml('data-test="connected"')
        ->assertDontSeeHtml('data-test="home-machine"');
});
