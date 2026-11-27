<?php

use App\Client\ProviderLimits;
use Carbon\CarbonImmutable;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(function () {
    connectClientWeb();
    CarbonImmutable::setTestNow('2026-11-14T20:00:00Z');
});

afterEach(fn () => CarbonImmutable::setTestNow());

/**
 * laptop is out of quota on cursor-sol until Nov 17 and was on codex until a time that has passed; workstation has no limits.
 *
 * @return array{nodes: list<array<string, mixed>>}
 */
function limitedNodes(): array
{
    return ['nodes' => [
        ['name' => 'laptop', 'revoked_at' => null, 'last_seen_at' => '2026-11-14T19:59:00Z', 'stale' => false, 'status' => ['reported_at' => '2026-11-14T19:59:00Z', 'active' => [], 'provider_limits' => [
            ['template' => 'cursor-sol', 'message' => 'You\'ve hit your usage limit', 'resets_at' => '2026-11-17T08:00:00Z'],
            ['template' => 'codex', 'message' => 'limit', 'resets_at' => '2026-11-14T19:00:00Z'],
        ]]],
        ['name' => 'workstation', 'revoked_at' => null, 'last_seen_at' => '2026-11-14T19:59:00Z', 'stale' => false, 'status' => ['reported_at' => '2026-11-14T19:59:00Z', 'active' => []]],
    ]];
}

/**
 * @return array<string, mixed>
 */
function limitedModels(): array
{
    return ['models' => [
        ['name' => 'cursor-sol', 'label' => 'Cursor Sol (subscription)', 'display_name' => 'Cursor Sol', 'provider' => 'cursor', 'billing' => 'subscription', 'available' => true, 'lead' => false],
        ['name' => 'codex', 'label' => 'Codex (subscription)', 'display_name' => 'Codex', 'provider' => 'openai', 'billing' => 'subscription', 'available' => true, 'lead' => false],
        ['name' => 'claude-code-sonnet', 'label' => 'Claude Sonnet (subscription)', 'display_name' => 'Claude Sonnet', 'provider' => 'anthropic', 'billing' => 'subscription', 'available' => true, 'lead' => true],
    ], 'default_model' => 'cursor-sol'];
}

it('reads each machine\'s out-of-quota models, dropping limits that have reset', function () {
    expect(ProviderLimits::fromNodes(limitedNodes()))->toBe(['laptop' => ['cursor-sol' => '2026-11-17T08:00:00Z']])
        ->and(ProviderLimits::phrase('2026-11-17T08:00:00Z'))->toBe('out of quota until Nov 17')
        ->and(ProviderLimits::phrase(null))->toBe('out of quota');
});

it('keeps a limit with no reset time, and ignores rows it cannot read', function () {
    $nodes = ['nodes' => [['name' => 'laptop', 'status' => ['provider_limits' => [['template' => 'cursor-sol'], ['template' => ''], 'junk', ['resets_at' => '2026-11-20T00:00:00Z']]]], 'junk']];

    expect(ProviderLimits::fromNodes($nodes))->toBe(['laptop' => ['cursor-sol' => null]]);
});

it('shows each machine\'s out-of-quota models on the machines page', function () {
    Http::fake([director('nodes') => Http::response(limitedNodes())]);
    fakeDirectorBackground();

    Livewire::test('pages::client.fleet')
        ->assertSeeText('cursor-sol out of quota until Nov 17')
        ->assertDontSeeText('codex out of quota');
});

it('marks out-of-quota models in the new task lane picker and does not preselect one', function () {
    Http::fake([
        director('nodes') => Http::response(limitedNodes()),
        director('models') => Http::response(limitedModels()),
        director('repos') => Http::response(['repos' => [['name' => 'contextstack']]]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.lane-create')
        ->assertSet('node', 'laptop')
        ->assertSet('model', 'auto')
        ->assertSet('defaultModel', 'codex')
        ->set('provider', 'cursor')
        ->assertSet('model', 'cursor-sol')
        ->assertSeeText('Cursor Sol (subscription) · subscription — out of quota until Nov 17')
        ->set('node', 'workstation')
        ->assertDontSeeText('out of quota');
});

it('marks out-of-quota models in the new conversation picker for the machine it would run on', function () {
    meOnDirector(['credential_id' => 'cred_local', 'owner' => true]);
    Http::fake([
        director('lead-node') => Http::response(['node' => 'workstation', 'source' => 'set']),
        director('nodes') => Http::response(limitedNodes()),
        director('models') => Http::response(limitedModels()),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
    ]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.conversation-create')
        ->assertSet('kind', 'lead')
        ->assertDontSeeText('out of quota');

    $page->set('kind', 'chat')->set('node', 'laptop')
        ->assertSeeText('— out of quota until Nov 17');
    expect($page->instance()->limitedHere())->toBe(['cursor-sol' => '2026-11-17T08:00:00Z']);
});
