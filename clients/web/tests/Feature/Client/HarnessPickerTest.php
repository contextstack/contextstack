<?php

use App\Client\HarnessCatalog;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * A GET models answer with the three-step `catalog` (CLIENT-API.md §4.5): Claude Sonnet runs in Claude Code or
 * Cursor (only the Claude Code pair can lead), the Codex CLI's default model has a null version, Gemini runs in
 * Antigravity (not signed in anywhere) or ContextStack's harness, an Ollama cloud model is hosted, and a role-only
 * planner row must stay hidden. The flat `models` stay as today.
 *
 * @return array<string, mixed>
 */
function harnessCatalogAnswer(): array
{
    $flat = fn (string $name, string $provider, bool $lead, array $over = []): array => [...[
        'name' => $name, 'provider' => $provider, 'provider_label' => ucfirst($provider), 'label' => $name, 'display_name' => $name,
        'nodes' => [], 'internal' => false, 'efforts' => [], 'available' => true, 'unavailable_reason' => null,
        'billing' => 'subscription', 'cost_class' => 'subscription', 'executor' => 'cli', 'lead' => $lead,
    ], ...$over];
    $harness = fn (string $id, string $label, array $providers, array $nodes, string $billing = 'subscription', ?string $reason = null): array => [
        'id' => $id, 'label' => $label, 'kind' => $id === 'contextstack' ? 'contextstack' : 'cli',
        'provider_ids' => $providers, 'nodes' => $nodes, 'billing' => $billing, 'unavailable_reason' => $reason,
    ];
    $model = fn (string $id, string $label, ?string $version, string $provider, array $pairs, bool $lead, array $over = []): array => [...[
        'id' => $id, 'label' => $label, 'version' => $version, 'provider' => $provider,
        'harnesses' => array_map(fn (array $pair): array => ['harness' => $pair[0], 'value' => $pair[1]], $pairs),
        'internal' => false, 'efforts' => [], 'lead' => $lead, 'available' => true, 'unavailable_reason' => null,
        'billing' => 'subscription', 'nodes' => [],
    ], ...$over];

    return [
        'models' => [
            $flat('claude-code-sonnet', 'anthropic', true, ['nodes' => ['node-a']]),
            $flat('cursor-sonnet', 'anthropic', false, ['nodes' => ['laptop']]),
            $flat('codex', 'openai', true, ['nodes' => ['node-a']]),
            $flat('cursor-sol', 'openai', false, ['nodes' => ['laptop']]),
            $flat('antigravity-hydrogen', 'google', false),
            $flat('gemini-4-hydrogen', 'google', false, ['billing' => 'paid_api', 'executor' => 'gateway']),
            $flat('minimax-m2.1:cloud', 'ollama', false, ['billing' => 'hosted', 'executor' => 'gateway']),
            $flat('planner', 'anthropic', false, ['internal' => true]),
        ],
        'default_model' => 'codex',
        'gateway' => 'http://127.0.0.1:1',
        'catalog' => [
            'providers' => [
                ['id' => 'anthropic', 'label' => 'Anthropic'],
                ['id' => 'openai', 'label' => 'OpenAI'],
                ['id' => 'google', 'label' => 'Google'],
                ['id' => 'ollama', 'label' => 'Ollama'],
            ],
            'harnesses' => [
                $harness('claude-code', 'Claude Code', ['anthropic'], ['node-a']),
                $harness('cursor-agent', 'Cursor', ['anthropic', 'openai'], ['laptop']),
                $harness('codex', 'Codex CLI', ['openai'], ['node-a']),
                $harness('antigravity', 'Antigravity', ['google'], [], 'subscription', 'Antigravity is not signed in on any machine'),
                $harness('contextstack', 'ContextStack', ['google', 'ollama'], ['node-a', 'node-b'], 'provider'),
            ],
            'models' => [
                $model('claude-sonnet-5.5', 'Claude Sonnet 5.5', 'claude-sonnet-5-5', 'anthropic',
                    [['claude-code', 'claude-code-sonnet'], ['cursor-agent', 'cursor-sonnet']], true),
                $model('planner', 'Planner role', null, 'anthropic', [['claude-code', 'planner']], false, ['internal' => true]),
                $model('codex-default', "Codex: the CLI's default model", null, 'openai', [['codex', 'codex']], true),
                $model('gpt-5.6-sol-high', 'GPT-5.6 Sol High', 'gpt-5.6-sol-high', 'openai', [['cursor-agent', 'cursor-sol']], false),
                $model('gemini-4-hydrogen', 'Gemini 4 Hydrogen', 'gemini-4-hydrogen-preview', 'google',
                    [['antigravity', 'antigravity-hydrogen'], ['contextstack', 'harness:gemini-4-hydrogen']], false, ['billing' => 'paid_api']),
                $model('minimax-m2.1:cloud', 'MiniMax M2.1 (hosted)', 'minimax-m2.1:cloud', 'ollama',
                    [['contextstack', 'harness:minimax-m2.1:cloud']], false, ['billing' => 'hosted']),
            ],
            'default' => ['harness' => 'codex', 'model' => 'codex-default', 'value' => 'codex'],
        ],
    ];
}

/**
 * @param  list<array<string, mixed>>  $limits  the laptop's `status.provider_limits`
 * @param  array<string, mixed>|null  $models  the GET models answer (default: harnessCatalogAnswer())
 */
function fakeHarnessDirector(array $limits = [], ?array $models = null): void
{
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null, 'status' => ['provider_limits' => $limits]]]]),
        director('lead-node') => Http::response(['node' => 'laptop']),
        director('models') => Http::response($models ?? harnessCatalogAnswer()),
        director('repos') => Http::response(['repos' => [['name' => 'contextstack']]]),
        director('projects') => Http::response(['projects' => []]),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('intents') => Http::response(['intent' => ['intent_id' => 'int_1'], 'jobs' => [['job_id' => 'job_1']], 'runs' => []], 201),
        director('conversations') => Http::response(['conversation' => ['conversation_id' => 'con_new']], 201),
    ]);
}

it('builds provider, model and harness steps from the catalog, hiding internal rows', function () {
    $catalog = HarnessCatalog::fromAnswer(harnessCatalogAnswer());
    expect($catalog)->not->toBeNull();

    $anthropic = HarnessCatalog::modelsOf($catalog, 'anthropic');
    $gemini = HarnessCatalog::pairsOf($catalog, 'google', 'gemini-4-hydrogen');
    $hosted = HarnessCatalog::pairsOf($catalog, 'ollama', 'minimax-m2.1:cloud');

    expect(array_column($catalog['providers'], 'id'))->toBe(['anthropic', 'openai', 'google', 'ollama'])
        ->and(array_column($anthropic, 'id'))->toBe(['claude-sonnet-5.5'])
        ->and($anthropic[0]['label'])->toBe('Claude Sonnet 5.5')
        ->and(HarnessCatalog::modelsOf($catalog, 'google')[0]['label'])->toBe('Gemini 4 Hydrogen (gemini-4-hydrogen-preview)')
        ->and(array_column(HarnessCatalog::modelsOf($catalog, 'openai'), 'label'))->toBe(["Codex: the CLI's default model", 'GPT-5.6 Sol High'])
        ->and(array_column($anthropic[0]['pairs'], 'option'))->toBe(['Claude Code · subscription · on node-a', 'Cursor · subscription · on laptop'])
        ->and($gemini[0]['available'])->toBeFalse()
        ->and($gemini[0]['option'])->toBe('Antigravity · subscription — Antigravity is not signed in on any machine')
        ->and($gemini[1]['option'])->toBe('ContextStack · paid API · on node-a, node-b')
        ->and($hosted[0]['option'])->toBe('ContextStack · hosted · on node-a, node-b')
        ->and(HarnessCatalog::readyValues($catalog))->not->toContain('planner')->not->toContain('antigravity-hydrogen')
        ->and(HarnessCatalog::defaultValue($catalog))->toBe('codex')
        ->and(HarnessCatalog::choose($catalog, 'google'))->toBe(['provider' => 'google', 'model' => 'gemini-4-hydrogen', 'harness' => 'contextstack', 'value' => 'harness:gemini-4-hydrogen'])
        ->and(HarnessCatalog::fromAnswer(['models' => [], 'default_model' => 'codex']))->toBeNull();
});

it('keeps only lead models and their lead-capable harnesses for a lead', function () {
    $lead = HarnessCatalog::forLead(HarnessCatalog::fromAnswer(harnessCatalogAnswer()));

    expect(array_column($lead['providers'], 'id'))->toBe(['anthropic', 'openai'])
        ->and(array_column($lead['models'], 'id'))->toBe(['claude-sonnet-5.5', 'codex-default'])
        ->and(array_column($lead['models'][0]['pairs'], 'harness'))->toBe(['claude-code'])
        ->and(HarnessCatalog::readyValues($lead))->toBe(['claude-code-sonnet', 'codex']);
});

it('keeps Auto as the new lane\'s default with a catalog, sent as auto, with no model or harness step', function () {
    fakeHarnessDirector();

    Livewire::test('pages::client.lane-create')
        ->assertSet('provider', 'auto')
        ->assertSet('model', 'auto')
        ->assertSeeText('Google')
        ->assertDontSee('data-test="lane-model"', false)
        ->assertDontSee('data-test="lane-harness"', false)
        ->set('title', 'Fix it')->set('goal', 'Make it work.')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['lane']['model'] === 'auto');
});

it('narrows provider, model, then harness on the new lane and sends the chosen pair\'s value', function () {
    fakeHarnessDirector();

    Livewire::test('pages::client.lane-create')
        ->set('provider', 'anthropic')
        ->assertSet('catalogModel', 'claude-sonnet-5.5')
        ->assertSet('harness', 'claude-code')
        ->assertSet('model', 'claude-code-sonnet')
        ->assertSee('data-test="lane-harness"', false)
        ->assertSeeText('Claude Sonnet 5.5')
        ->assertSeeText('Cursor · subscription · on laptop')
        ->assertDontSeeText('Planner role')
        ->assertDontSeeText('GPT-5.6 Sol High')
        ->set('harness', 'cursor-agent')
        ->assertSet('model', 'cursor-sonnet')
        ->set('title', 'Fix it')->set('goal', 'Make it work.')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['lane']['model'] === 'cursor-sonnet');
});

it('shows a single harness as text and switches models within a provider', function () {
    fakeHarnessDirector();

    Livewire::test('pages::client.lane-create')
        ->set('provider', 'openai')
        ->assertSet('catalogModel', 'codex-default')
        ->assertSet('model', 'codex')
        ->assertDontSee('data-test="lane-harness"', false)
        ->assertSee('data-test="lane-harness-text"', false)
        ->assertSeeText('Runs in Codex CLI · subscription · on node-a')
        ->set('catalogModel', 'gpt-5.6-sol-high')
        ->assertSet('harness', 'cursor-agent')
        ->assertSet('model', 'cursor-sol')
        ->assertSeeText('Runs in Cursor · subscription · on laptop')
        ->set('title', 'Fix it')->set('goal', 'Make it work.')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['lane']['model'] === 'cursor-sol');
});

it('skips an unavailable harness and sends a ContextStack harness value exactly as given', function () {
    fakeHarnessDirector();

    Livewire::test('pages::client.lane-create')
        ->set('provider', 'google')
        ->assertSet('harness', 'contextstack')
        ->assertSet('model', 'harness:gemini-4-hydrogen')
        ->assertSeeText('Antigravity · subscription — Antigravity is not signed in on any machine')
        ->set('harness', 'antigravity')
        ->assertSet('harness', 'contextstack')
        ->set('title', 'Fix it')->set('goal', 'Make it work.')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['lane']['model'] === 'harness:gemini-4-hydrogen');
});

it('prefers a harness the machine is not out of quota for, and says so beside the limited one', function () {
    fakeHarnessDirector([['template' => 'claude-code-sonnet', 'message' => 'limit', 'resets_at' => null]]);

    Livewire::test('pages::client.lane-create')
        ->set('provider', 'anthropic')
        ->assertSet('harness', 'cursor-agent')
        ->assertSet('model', 'cursor-sonnet')
        ->assertSeeText('Claude Code · subscription · on node-a — out of quota');
});

it('runs a scratch lane on the catalog default value', function () {
    fakeHarnessDirector();

    Livewire::test('pages::client.lane-create')
        ->set('scratch', true)
        ->assertSet('model', 'codex')
        ->assertSet('provider', 'openai')
        ->assertSet('harness', 'codex')
        ->set('title', 'Try it')->set('goal', 'Explore.')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['lane']['model'] === 'codex');
});

it('offers only lead models and lead harnesses when the conversation runs tasks', function () {
    fakeHarnessDirector();

    Livewire::test('pages::client.conversation-create')
        ->assertSet('kind', 'lead')
        ->assertSet('model', 'codex')
        ->assertSet('provider', 'openai')
        ->assertSet('catalogModel', 'codex-default')
        ->assertDontSeeText('GPT-5.6 Sol High')
        ->assertDontSeeText('Google')
        ->set('provider', 'anthropic')
        ->assertSet('model', 'claude-code-sonnet')
        ->assertDontSee('data-test="harness-select"', false)
        ->assertSeeText('Runs in Claude Code · subscription · on node-a')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('conversations')
        && $request['spec'] === 'hosted-lead' && $request['model'] === 'claude-code-sonnet');
});

it('starts a chat on any catalog pair and sends its value', function () {
    fakeHarnessDirector();

    Livewire::test('pages::client.conversation-create')
        ->set('kind', 'chat')
        ->assertSet('model', 'codex')
        ->set('provider', 'anthropic')
        ->assertSee('data-test="harness-select"', false)
        ->set('harness', 'cursor-agent')
        ->assertSet('model', 'cursor-sonnet')
        ->set('provider', 'ollama')
        ->assertSet('model', 'harness:minimax-m2.1:cloud')
        ->assertSeeText('Runs in ContextStack · hosted · on node-a, node-b')
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('conversations')
        && $request['spec'] === 'chat' && $request['model'] === 'harness:minimax-m2.1:cloud');
});

it('falls back to the two-step picker when GET models has no catalog', function () {
    $answer = harnessCatalogAnswer();
    unset($answer['catalog']);
    fakeHarnessDirector(models: $answer);

    Livewire::test('pages::client.lane-create')
        ->assertSet('catalog', null)
        ->set('provider', 'anthropic')
        ->assertSet('model', 'claude-code-sonnet')
        ->assertSee('data-test="lane-model"', false)
        ->assertDontSee('data-test="lane-harness"', false)
        ->assertDontSee('data-test="lane-harness-text"', false);
});
