<?php

use App\Client\ModelCatalog;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * A GET models answer in the §4.5 shape: two Anthropic models (both can lead), two OpenAI models (Codex can lead),
 * one local model, and a role-only planner row the pickers must hide.
 *
 * @return array<string, mixed>
 */
function pickerCatalog(): array
{
    $row = fn (string $name, string $provider, string $providerLabel, string $label, bool $lead, array $over = []): array => [...[
        'name' => $name, 'provider' => $provider, 'provider_label' => $providerLabel, 'label' => $label, 'display_name' => 'legacy '.$name,
        'nodes' => ['laptop'], 'internal' => false, 'efforts' => [], 'available' => true, 'unavailable_reason' => null,
        'billing' => 'subscription', 'cost_class' => 'subscription', 'executor' => $name, 'lead' => $lead,
    ], ...$over];

    return ['models' => [
        $row('claude-code-sonnet', 'anthropic', 'Anthropic', 'Claude Sonnet 4.5 via Claude Code', true),
        $row('claude-code-opus', 'anthropic', 'Anthropic', 'Claude Opus 4.5 via Claude Code', true),
        $row('codex', 'openai', 'OpenAI', 'GPT-5.1 Codex via Codex CLI', true),
        $row('gpt-5-api', 'openai', 'OpenAI', 'GPT-5 (API)', false, ['billing' => 'paid_api', 'nodes' => []]),
        $row('local-coder', 'ollama', 'Local (Ollama)', 'Qwen3 Coder 30B', false, ['billing' => 'free_local', 'nodes' => ['node-a', 'node-b']]),
        $row('planner', 'anthropic', 'Anthropic', 'Planner role', false, ['internal' => true]),
    ], 'default_model' => 'codex', 'gateway' => 'http://127.0.0.1:1'];
}

function fakePickerDirector(): void
{
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(pickerCatalog()),
        director('repos') => Http::response(['repos' => [['name' => 'contextstack']]]),
        director('projects') => Http::response(['projects' => []]),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('intents') => Http::response(['intent' => ['intent_id' => 'int_1'], 'jobs' => [['job_id' => 'job_1']], 'runs' => []], 201),
        director('conversations') => Http::response(['conversation' => ['conversation_id' => 'con_new']], 201),
    ]);
    fakeDirectorBackground();
}

it('hides role-only rows and groups the rest by provider with the catalog\'s provider label', function () {
    $choices = ModelCatalog::parse(pickerCatalog())['choices'];
    $groups = ModelCatalog::groups($choices);

    expect(array_column($choices, 'name'))->not->toContain('planner')
        ->and(array_column($groups, 'provider'))->toBe(['anthropic', 'openai', 'ollama'])
        ->and(array_column($groups, 'label'))->toBe(['Anthropic', 'OpenAI', 'Local (Ollama)'])
        ->and(array_column($groups[1]['models'], 'name'))->toBe(['codex', 'gpt-5-api'])
        ->and($groups[2]['models'][0]['option'])->toBe('Qwen3 Coder 30B · free on this Mac · runs on node-a, node-b')
        ->and($groups[1]['models'][1]['option'])->toBe('GPT-5 (API) · paid API')
        ->and(ModelCatalog::providerOf('local-coder', $choices))->toBe('ollama')
        ->and(ModelCatalog::firstReadyOf('anthropic', $choices))->toBe('claude-code-sonnet')
        ->and(ModelCatalog::firstReadyOf('anthropic', $choices, ['claude-code-sonnet' => null]))->toBe('claude-code-opus')
        ->and(ModelCatalog::firstReadyOf('nobody', $choices))->toBe('');
});

it('keeps Auto as the new lane\'s default, sent as auto, with no model step', function () {
    fakePickerDirector();

    Livewire::test('pages::client.lane-create')
        ->assertSet('provider', 'auto')
        ->assertSet('model', 'auto')
        ->assertSeeText('Auto (plans first, then picks the model)')
        ->assertSeeText('Anthropic')
        ->assertDontSee('data-test="lane-model"', false)
        ->assertDontSeeText('Planner role')
        ->set('title', 'Fix it')->set('goal', 'Make it work.')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['lane']['model'] === 'auto');
});

it('picks a provider on the new lane, which selects its first ready model and sends it', function () {
    fakePickerDirector();

    Livewire::test('pages::client.lane-create')
        ->set('provider', 'openai')
        ->assertSet('model', 'codex')
        ->assertSee('data-test="lane-model"', false)
        ->assertSeeText('GPT-5.1 Codex via Codex CLI · subscription · runs on laptop')
        ->assertDontSeeText('Claude Opus 4.5')
        ->set('provider', 'anthropic')
        ->assertSet('model', 'claude-code-sonnet')
        ->set('model', 'claude-code-opus')
        ->set('title', 'Fix it')->set('goal', 'Make it work.')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['lane']['model'] === 'claude-code-opus');
});

it('switches the new lane back to Auto from a provider', function () {
    fakePickerDirector();

    Livewire::test('pages::client.lane-create')
        ->set('provider', 'ollama')->assertSet('model', 'local-coder')
        ->set('provider', 'auto')->assertSet('model', 'auto')
        ->assertDontSee('data-test="lane-plan-with"', false);
});

it('starts a chat on the default model\'s provider and sends the model a provider switch selects', function () {
    fakePickerDirector();

    Livewire::test('pages::client.conversation-create')
        ->set('kind', 'chat')
        ->assertSet('model', 'codex')
        ->assertSet('provider', 'openai')
        ->assertSeeText('GPT-5 (API) · paid API')
        ->assertDontSeeText('Planner role')
        ->set('provider', 'ollama')
        ->assertSet('model', 'local-coder')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('conversations')
        && $request['spec'] === 'chat' && $request['model'] === 'local-coder');
});

it('offers only lead models when the conversation runs tasks', function () {
    fakePickerDirector();

    $page = Livewire::test('pages::client.conversation-create')
        ->assertSet('kind', 'lead')
        ->assertSet('model', 'codex')
        ->assertSet('provider', 'openai')
        ->assertDontSeeText('Local (Ollama)')
        ->assertDontSeeText('GPT-5 (API)');

    expect(array_column($page->instance()->modelGroups(), 'provider'))->toBe(['anthropic', 'openai'])
        ->and(array_column($page->instance()->modelGroups()[1]['models'], 'name'))->toBe(['codex']);

    $page->set('provider', 'anthropic')
        ->assertSet('model', 'claude-code-sonnet')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('conversations')
        && $request['spec'] === 'hosted-lead' && $request['model'] === 'claude-code-sonnet');
});
