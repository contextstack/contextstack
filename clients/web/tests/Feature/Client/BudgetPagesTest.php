<?php

use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * A budget as the Director shows it (CLIENT-API.md §12.2).
 *
 * @return array<string, mixed>
 */
function budgetShown(string $state, array $over = []): array
{
    $base = match ($state) {
        'none' => ['shown' => 'no budget set', 'usd' => null, 'remaining_usd' => null],
        'active' => ['shown' => '$3.75 of $5.00 left', 'usd' => 5.0, 'spent_usd' => 1.25, 'reserved_usd' => 0.0, 'remaining_usd' => 3.75],
        'reserved' => ['shown' => 'all that remains is reserved for running work', 'usd' => 5.0, 'spent_usd' => 0.0, 'reserved_usd' => 5.0, 'remaining_usd' => 0.0],
        'exhausted' => ['shown' => 'budget used up', 'usd' => 1.0, 'spent_usd' => 1.0, 'reserved_usd' => 0.0, 'remaining_usd' => 0.0],
    };

    return [...['state' => $state, 'period' => 'total', 'resets_at' => null, 'waiting_job_ids' => [], 'set_by' => 'cred_local', 'set_at' => '2026-11-10T10:00:00Z'], ...$base, ...$over];
}

/**
 * @return array<string, mixed>
 */
function projectRecord(array $over = []): array
{
    return [...['project_id' => 'prj_1', 'name' => 'site rebuild', 'created_by' => 'cred_local', 'created_at' => '2026-11-10T10:00:00Z',
        'budget' => budgetShown('none'), 'revision' => 120], ...$over];
}

function fakeProject(array $project, array $more = []): void
{
    Http::fake([
        director('projects/prj_1') => Http::response(['project' => $project]),
        director('intents?*') => Http::response(['intents' => [
            ['intent_id' => 'int_1', 'title' => 'in the project', 'task' => 'tasks/a', 'node' => 'laptop', 'project_id' => 'prj_1', 'created_at' => '2026-11-10T10:00:00Z', 'revision' => 3],
            ['intent_id' => 'int_2', 'title' => 'elsewhere', 'task' => 'tasks/b', 'node' => 'laptop', 'project_id' => null, 'created_at' => '2026-11-10T10:00:00Z', 'revision' => 4],
        ], 'jobs' => []]),
        director('conversations?*') => Http::response(['conversations' => [
            conversationRecord(['conversation_id' => 'con_1', 'title' => 'project chat', 'project_id' => 'prj_1']),
            conversationRecord(['conversation_id' => 'con_2', 'title' => 'other chat', 'project_id' => null]),
        ]]),
        director('usage*') => Http::response(['by' => 'project_id', 'period' => 'all', 'groups' => [], 'total' => ['runs' => 0, 'kinds' => []]]),
        ...$more,
    ]);
}

it('creates a project with a budget under a key and opens it', function () {
    Http::fake([
        director('projects') => Http::sequence()->push(['projects' => []])
            ->push(['project' => projectRecord(['budget' => budgetShown('active')])], 201),
    ]);

    $page = Livewire::test('pages::client.projects')->assertSee('No projects yet.');
    $key = $page->get('createKey');
    $page->set('name', 'site rebuild')->set('budgetUsd', '5')->set('budgetPeriod', 'month')->call('create')
        ->assertHasNoErrors()->assertRedirect(route('projects.show', 'prj_1'));

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('projects')
        && $request->hasHeader('Idempotency-Key', $key) && $request['name'] === 'site rebuild'
        && $request['budget'] === ['usd' => '5', 'period' => 'month']);
});

it('creates a project without a budget when none is given', function () {
    Http::fake([director('projects') => Http::sequence()->push(['projects' => []])->push(['project' => projectRecord()], 201)]);

    Livewire::test('pages::client.projects')->set('name', 'free')->call('create');

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && ! isset($request['budget']));
});

it('lists projects, each budget state shown distinctly: no budget set is never budget used up', function () {
    Http::fake([director('projects') => Http::response(['projects' => [
        projectRecord(['project_id' => 'prj_none', 'name' => 'free']),
        projectRecord(['project_id' => 'prj_active', 'name' => 'roomy', 'budget' => budgetShown('active')]),
        projectRecord(['project_id' => 'prj_reserved', 'name' => 'busy', 'budget' => budgetShown('reserved', ['waiting_job_ids' => ['job_9']])]),
        projectRecord(['project_id' => 'prj_spent', 'name' => 'spent', 'budget' => budgetShown('exhausted')]),
    ]])]);

    $html = $this->get(route('projects.index'))->assertOk()->getContent();

    foreach (['none', 'active', 'reserved', 'exhausted'] as $state) {
        expect($html)->toContain('data-test="budget" data-state="'.$state.'"');
    }
    expect($html)->toContain('no budget set')->toContain('$3.75 of $5.00 left')
        ->toContain('all that remains is reserved for running work')->toContain('budget used up');
});

it('shows a project\'s budget, the jobs it holds back with their reasons, and the intents and conversations that name it', function () {
    fakeProject(projectRecord(['budget' => budgetShown('reserved', ['waiting_job_ids' => ['job_w1', 'job_w2']])]), [
        director('jobs/job_w1') => Http::response(['job_id' => 'job_w1', 'intent_id' => 'int_1', 'state' => 'waiting', 'conversation_id' => null, 'revision' => 8,
            'budget' => ['state' => 'waiting', 'project_id' => 'prj_1', 'reason' => 'budget_reserved', 'detail' => 'waiting: $5.00 is reserved for running work']]),
        director('jobs/job_w2') => Http::response(['job_id' => 'job_w2', 'intent_id' => null, 'state' => 'waiting', 'conversation_id' => 'con_1', 'revision' => 9,
            'budget' => ['state' => 'waiting', 'project_id' => 'prj_1', 'reason' => 'budget_exhausted', 'detail' => 'budget used up until it is raised']]),
    ]);

    $this->get(route('projects.show', 'prj_1'))->assertOk()
        ->assertSee('site rebuild')->assertSee('data-state="reserved"', false)
        ->assertSee('2 jobs wait')->assertSee('waiting: $5.00 is reserved for running work')->assertSee('room reserved for running work')
        ->assertSee('budget used up until it is raised')
        ->assertSee('in the project')->assertDontSee('elsewhere')->assertSee('project chat')->assertDontSee('other chat')
        ->assertSee(route('intents.create', ['project' => 'prj_1']), false)->assertSee(route('conversations.create', ['project' => 'prj_1']), false);

    Http::assertSent(fn ($request) => $request->url() === director('intents?project=prj_1'));
    Http::assertSent(fn ($request) => $request->url() === director('conversations?project=prj_1'));
});

it('sets and clears a project\'s budget without a key, offering what now fits', function () {
    fakeProject(projectRecord(['budget' => budgetShown('exhausted')]), [
        director('projects/prj_1/budget') => Http::sequence()
            ->push(['project' => projectRecord(['budget' => budgetShown('active', ['period' => 'month']), 'revision' => 130])])
            ->push(['project' => projectRecord(['revision' => 131])]),
    ]);

    $page = Livewire::test('pages::client.project', ['project' => 'prj_1'])->assertSee('budget used up');
    $page->set('budget.usd', '2.00')->set('budget.period', 'month')->call('setBudget')->assertSee('$3.75 of $5.00 left')->assertDontSee('budget used up');
    $page->call('clearBudget')->assertSee('no budget set');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->url() === director('projects/prj_1/budget')
        && $request['usd'] === '2.00' && $request['period'] === 'month' && ! $request->hasHeader('Idempotency-Key'));
    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && $request->url() === director('projects/prj_1/budget'));
});

it('refuses a negative budget before asking, and shows the Director\'s refusal of a bad one', function () {
    fakeProject(projectRecord(), [
        director('projects/prj_1/budget') => Http::response(['error' => 'Unprocessable', 'code' => 'invalid_request', 'detail' => 'usd must be 0 or more'], 422),
    ]);

    $page = Livewire::test('pages::client.project', ['project' => 'prj_1']);
    $page->set('budget.usd', '-1')->call('setBudget')->assertHasErrors(['budget.usd']);
    $page->set('budget.usd', '1')->set('budget.period', 'week')->call('setBudget')->assertHasErrors(['budget.period']);
    Http::assertNotSent(fn (Request $request) => $request->method() === 'PUT');
});

it('attaches an intent to a project by naming it when the intent is submitted', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('projects') => Http::response(['projects' => [['project_id' => 'prj_1', 'name' => 'site rebuild']]]),
        director('objects/*') => Http::response(['stored' => true]),
        director('intents') => Http::response(['intent' => intentRecord(['project_id' => 'prj_1']), 'jobs' => [jobRecord(['state' => 'waiting'])], 'runs' => []], 201),
    ]);

    Livewire::withQueryParams(['project' => 'prj_1'])->test('pages::client.intent-create')
        ->assertSet('projectId', 'prj_1')->assertSee('site rebuild')
        ->set('goal', 'Say hello.')->call('submit')->assertRedirect(route('intents.show', 'int_1'));

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['project_id'] === 'prj_1');
});

it('shows usage by source, each for what it is, and never an API-price estimate', function () {
    Http::fake([
        director('projects') => Http::response(['projects' => []]),
        director('usage*') => Http::response(['by' => 'source', 'period' => 'day', 'from' => '2026-11-10T05:00:00Z', 'to' => '2026-11-11T05:00:00Z',
            'groups' => [
                ['source' => ['class' => 'api', 'model' => 'gpt-test'], 'kind' => 'pay_as_you_go', 'runs' => 2, 'charged_usd' => 0.4, 'charge_unreported_runs' => 0,
                    'input_tokens' => 1500, 'output_tokens' => 300, 'est_usd' => 7.77, 'reported_models' => [], 'shown' => 'pay as you go: $0.40 charged'],
                ['source' => ['class' => 'subscription', 'model' => 'opus'], 'kind' => 'subscription', 'runs' => 1, 'input_tokens' => 900, 'allowance_reached' => true,
                    'allowance' => ['rejected' => true, 'reported_at' => '2026-11-10T11:00:00Z'], 'reported_models' => ['claude-opus-5-5'], 'shown' => 'subscription: allowance reached'],
                ['source' => ['class' => 'free_local', 'model' => 'local-coder'], 'kind' => 'local', 'runs' => 4, 'input_tokens' => 4000, 'reported_models' => [], 'shown' => 'local: usage only, no cost'],
            ],
            'total' => ['runs' => 7, 'charged_usd' => 0.4, 'kinds' => []]]),
    ]);

    $page = $this->get(route('usage'))->assertOk();
    $html = $page->getContent();

    $page->assertSee('$0.40 charged')->assertSee('subscription: allowance reached')->assertSee('allowance reached')->assertSee('usage only, no cost')
        ->assertSee('claude-opus-5-5')->assertDontSee('7.77');
    preg_match('/data-kind="subscription">(.*?)<\/div>/s', $html, $subscription);
    preg_match('/data-kind="local">(.*?)<\/div>/s', $html, $local);
    expect($subscription[1] ?? '')->not->toContain('$')
        ->and($local[1] ?? '')->not->toContain('$');
    Http::assertSent(fn (Request $request) => str_starts_with($request->url(), director('usage')) && $request['by'] === 'source' && $request['period'] === 'day');
});

it('asks for usage by project, conversation, intent or node over a month or all time, with filters', function () {
    Http::fake([
        director('projects') => Http::response(['projects' => [['project_id' => 'prj_1', 'name' => 'site rebuild']]]),
        director('usage*') => Http::response(['by' => 'project_id', 'period' => 'month', 'groups' => [
            ['project_id' => 'prj_1', 'runs' => 3, 'charged_usd' => 0.4, 'kinds' => [
                'pay_as_you_go' => ['runs' => 1, 'charged_usd' => 0.4, 'shown' => 'pay as you go: $0.40 charged'],
                'local' => ['runs' => 2, 'shown' => 'local: usage only, no cost'],
            ]],
            ['project_id' => null, 'runs' => 1, 'charged_usd' => 0, 'kinds' => ['local' => ['runs' => 1, 'shown' => 'local: usage only, no cost']]],
        ], 'total' => ['runs' => 4, 'charged_usd' => 0.4, 'kinds' => []]]),
    ]);

    Livewire::withQueryParams(['node' => 'laptop'])->test('pages::client.usage')
        ->set('by', 'project')->set('period', 'month')
        ->assertSee('site rebuild')->assertSee('(none)')->assertSee('$0.40 charged')->assertSee('machine:');
    Livewire::test('pages::client.usage')->set('period', 'all')->set('date', '2026-11');

    Http::assertSent(fn (Request $request) => str_starts_with($request->url(), director('usage')) && $request['by'] === 'project'
        && $request['period'] === 'month' && $request['node'] === 'laptop');
    Http::assertNotSent(fn (Request $request) => str_starts_with($request->url(), director('usage')) && $request['period'] === 'all' && isset($request['date']));
});

it('shows source budgets labelled, off distinct from used up, and lets the owner set and turn one off', function () {
    $report = fn (string $openaiState, string $shown) => ['gateway' => '127.0.0.1:9200', 'budgets' => ['schema_version' => 'csgateway.budgets.v1', 'total' => null, 'sources' => [
        ['source' => 'anthropic', 'state' => 'off', 'budget' => null, 'shared' => false, 'spent' => ['day_usd' => 0, 'month_usd' => 0], 'shown' => 'off, set a budget to use'],
        ['source' => 'openai', 'state' => $openaiState, 'budget' => $openaiState === 'off' ? null : ['usd' => 5.0, 'period' => 'day'], 'shared' => false,
            'spent' => ['day_usd' => $openaiState === 'exhausted' ? 5.0 : 0, 'month_usd' => 0], 'shown' => $shown],
    ]]];
    Http::fake([
        director('me') => Http::response(['credential_id' => 'cred_local', 'owner' => true]),
        director('source-budgets') => Http::response($report('exhausted', 'budget used up until 2026-11-11T05:00:00Z')),
        director('source-budgets/*') => Http::sequence()->push($report('active', 'active: $5.00 left'))->push($report('off', 'off, set a budget to use')),
    ]);

    $page = Livewire::test('pages::client.sources')
        ->assertSee('set a budget to use')->assertDontSeeText('off, set a budget to use')->assertSee('until 2026-11-11T05:00:00Z')->assertDontSee('This device is paired');
    expect($page->html())->toContain('data-source="anthropic" data-state="off"')->toContain('data-source="openai" data-state="exhausted"');

    $page->set('source', 'openai')->set('usd', '5')->set('period', 'day')->call('setSource')->assertSee('$5.00 left')->assertDontSeeText('active: $5.00 left');
    $page->call('turnOff', 'openai')->assertDontSee('$5.00 left');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->url() === director('source-budgets/openai')
        && $request['usd'] === '5' && $request['period'] === 'day' && ! $request->hasHeader('Idempotency-Key'));
    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && $request->url() === director('source-budgets/openai'));
});

it('shows the Director\'s 403 owner_only clearly when a paired device sets a source budget', function () {
    Http::fake([
        director('me') => Http::response(['credential_id' => 'cred_phone', 'owner' => false]),
        director('source-budgets') => Http::response(['gateway' => '127.0.0.1:9200', 'budgets' => ['total' => null, 'sources' => [
            ['source' => 'openai', 'state' => 'off', 'budget' => null, 'shared' => false, 'spent' => ['day_usd' => 0, 'month_usd' => 0], 'shown' => 'off, set a budget to use'],
        ]]]),
        director('source-budgets/*') => Http::response(['error' => 'Forbidden', 'code' => 'owner_only', 'detail' => 'only the owner may do that'], 403),
    ]);

    Livewire::test('pages::client.sources')->assertSee('This device is paired, not the owner')
        ->set('source', '*')->set('usd', '5')->call('setSource')
        ->assertSee('The Director refused: 403 owner_only')->assertSee('this device is paired, and a paired device may not')
        ->assertSee('data-code="owner_only"', false);

    Http::assertSent(fn (Request $request) => $request->url() === director('source-budgets/%2A'));
});

it('shows a gateway the Director cannot reach as the Director says', function () {
    Http::fake([
        director('me') => Http::response(['credential_id' => 'cred_local', 'owner' => true]),
        director('source-budgets') => Http::response(['error' => 'Bad Gateway', 'code' => 'gateway_unavailable', 'detail' => 'connection refused'], 502),
    ]);

    Livewire::test('pages::client.sources')->assertSee('The Director could not reach its gateway: connection refused');
});

it('reads only the Director on the new pages, never this app\'s database', function () {
    Http::fake([
        director('conversations/con_1/thread') => Http::response(['conversation' => conversationRecord(), 'turns' => [], 'summaries' => []]),
        director('conversations?*') => Http::response(['conversations' => []]),
        director('projects/prj_1') => Http::response(['project' => projectRecord()]),
        director('projects') => Http::response(['projects' => []]),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('nodes') => Http::response(['nodes' => []]),
        director('usage*') => Http::response(['groups' => [], 'total' => ['runs' => 0, 'kinds' => []]]),
        director('me') => Http::response(['owner' => true]),
        director('source-budgets') => Http::response(['budgets' => ['sources' => [], 'total' => null]]),
    ]);
    // No database is configured (phpunit.xml names a connection that does not exist), so any read of one fails.

    foreach ([route('conversations.index'), route('conversations.create'), route('conversations.show', 'con_1'), route('projects.index'),
        route('projects.show', 'prj_1'), route('usage'), route('sources')] as $url) {
        $this->get($url)->assertOk();
    }
});
