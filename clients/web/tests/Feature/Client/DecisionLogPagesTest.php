<?php

use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * A decision log entry as the Director shows it (CLIENT-API.md "Project decision log").
 *
 * @return array<string, mixed>
 */
function decisionEntry(array $over = []): array
{
    return [...[
        'entry_id' => 'dle_1', 'project_id' => 'prj_cs', 'number' => 'D3', 'status' => 'active', 'superseded_by' => null,
        'decision' => 'The Director is the single source of record', 'why' => 'Private client state drifts.',
        'alternatives' => [], 'reopen_when' => ['A client needs to work offline'], 'sources' => ['ADR 0012'],
        'created_at' => '2026-11-21T10:00:00Z', 'created_by' => 'cred_local', 'updated_at' => '2026-11-21T10:00:00Z', 'updated_by' => 'cred_local',
        'revision' => 4,
    ], ...$over];
}

/**
 * @return array<string, mixed>
 */
function decisionProject(): array
{
    return [director('projects/prj_cs') => Http::response(['project' => ['project_id' => 'prj_cs', 'name' => 'ContextStack', 'revision' => 1]])];
}

it('lists the active log in number order and flags entries without alternatives', function () {
    Http::fake([
        ...decisionProject(),
        director('projects/prj_cs/decision_log*') => Http::response(['entries' => [
            decisionEntry(['entry_id' => 'dle_12', 'number' => 'D12', 'decision' => 'No backward compatibility in renames', 'alternatives' => [['option' => 'Aliases', 'why_not' => 'They pile up.']]]),
            decisionEntry(),
        ], 'markdown' => '']),
    ]);

    Livewire::test('pages::client.decision-log', ['project' => 'prj_cs'])
        ->assertSeeInOrder(['D3', 'The Director is the single source of record', 'No alternatives recorded', 'D12', 'No backward compatibility in renames', '1 alternative']);

    Http::assertSent(fn (Request $request) => $request->url() === director('projects/prj_cs/decision_log?status=active'));
});

it('asks the Director to search and filter, and says when nothing matches', function () {
    Http::fake([
        ...decisionProject(),
        director('projects/prj_cs/decision_log*') => Http::response(['entries' => [], 'markdown' => '']),
    ]);

    Livewire::test('pages::client.decision-log', ['project' => 'prj_cs'])
        ->set('status', 'all')
        ->set('search', 'offline')
        ->assertSee('No decisions match.');

    Http::assertSent(fn (Request $request) => $request->url() === director('projects/prj_cs/decision_log?q=offline'));
});

it('records a new decision with alternatives under a key and opens it', function () {
    Http::fake([
        ...decisionProject(),
        director('projects/prj_cs/decision_log?*') => Http::response(['entries' => [], 'markdown' => '']),
        director('projects/prj_cs/decision_log') => Http::response(['entry' => decisionEntry(['entry_id' => 'dle_new']), 'revisions' => [], 'markdown' => ''], 201),
    ]);

    $page = Livewire::test('pages::client.decision-log', ['project' => 'prj_cs'])->call('newDecision');
    $key = $page->get('createKey');
    $page->set('form.decision', 'Ship the desktop app')
        ->set('form.why', 'People want a window.')
        ->call('addAlternative')
        ->set('form.alternatives.0.option', 'Browser only')
        ->set('form.alternatives.0.why_not', 'Easy to lose the tab.')
        ->set('form.reopenWhen', "Electron gets too heavy\n\n")
        ->call('create')
        ->assertHasNoErrors()
        ->assertRedirect(route('decisions.show', ['project' => 'prj_cs', 'entry' => 'dle_new']));

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('projects/prj_cs/decision_log')
        && $request->hasHeader('Idempotency-Key', $key)
        && $request['alternatives'] === [['option' => 'Browser only', 'why_not' => 'Easy to lose the tab.']]
        && $request['reopen_when'] === ['Electron gets too heavy'] && $request['sources'] === []);
});

it('refuses a decision on more than one line, and an alternative without its reason', function () {
    Http::fake([...decisionProject(), director('projects/prj_cs/decision_log*') => Http::response(['entries' => [], 'markdown' => ''])]);

    Livewire::test('pages::client.decision-log', ['project' => 'prj_cs'])
        ->set('form.decision', "one\ntwo")
        ->set('form.why', 'because')
        ->call('addAlternative')
        ->set('form.alternatives.0.option', 'Something')
        ->call('create')
        ->assertHasErrors(['form.decision', 'form.alternatives.0.why_not']);

    Http::assertNotSent(fn (Request $request) => $request->method() === 'POST');
});

it('shows an entry with its history, and revises only what changed against the revision shown', function () {
    $earlier = decisionEntry(['why' => 'Drift.', 'revision' => 3]);
    $revised = decisionEntry(['alternatives' => [['option' => 'Client caches', 'why_not' => 'They disagree.']], 'revision' => 5]);
    Http::fake([
        ...decisionProject(),
        director('projects/prj_cs/decision_log/dle_1/revise') => Http::response(['entry' => $revised, 'revisions' => [$earlier, decisionEntry()], 'markdown' => '']),
        director('projects/prj_cs/decision_log/dle_1') => Http::response(['entry' => decisionEntry(), 'revisions' => [$earlier], 'markdown' => '']),
        director('projects/prj_cs/decision_log?*') => Http::response(['entries' => [decisionEntry()], 'markdown' => '']),
    ]);

    $page = Livewire::test('pages::client.decision', ['project' => 'prj_cs', 'entry' => 'dle_1'])
        ->assertSee('Private client state drifts.')
        ->assertSee('A client needs to work offline')
        ->assertSeeInOrder(['History', 'v2', 'changed why', '(current)', 'v1', 'recorded'])
        ->call('edit')
        ->call('addAlternative')
        ->set('form.alternatives.0.option', 'Client caches')
        ->set('form.alternatives.0.why_not', 'They disagree.')
        ->call('save')
        ->assertSet('editing', false)
        ->assertSee('Client caches');

    Http::assertSent(fn (Request $request) => $request->url() === director('projects/prj_cs/decision_log/dle_1/revise')
        && $request->data() === ['alternatives' => [['option' => 'Client caches', 'why_not' => 'They disagree.']], 'revision' => 4]);
});

it('shows a stale revise as changed elsewhere and keeps what the Director holds', function () {
    Http::fake([
        ...decisionProject(),
        director('projects/prj_cs/decision_log/dle_1/revise') => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'stale revision'], 409),
        director('projects/prj_cs/decision_log/dle_1') => Http::sequence()
            ->push(['entry' => decisionEntry(), 'revisions' => [], 'markdown' => ''])
            ->push(['entry' => decisionEntry(['why' => 'Changed by the lead.', 'revision' => 6]), 'revisions' => [], 'markdown' => '']),
        director('projects/prj_cs/decision_log?*') => Http::response(['entries' => [], 'markdown' => '']),
    ]);

    Livewire::test('pages::client.decision', ['project' => 'prj_cs', 'entry' => 'dle_1'])
        ->call('edit')
        ->set('form.why', 'Mine.')
        ->call('save')
        ->assertSee('This decision changed since you opened it')
        ->assertSet('editing', false)
        ->assertSee('Changed by the lead.');
});

it('supersedes by another active entry and retires against the revision shown', function () {
    Http::fake([
        ...decisionProject(),
        director('projects/prj_cs/decision_log/dle_1/supersede') => Http::response(['entry' => decisionEntry(['status' => 'superseded', 'superseded_by' => 'dle_9', 'revision' => 5]), 'revisions' => [], 'markdown' => '']),
        director('projects/prj_cs/decision_log/dle_1') => Http::response(['entry' => decisionEntry(), 'revisions' => [], 'markdown' => '']),
        director('projects/prj_cs/decision_log?*') => Http::response(['entries' => [decisionEntry(), decisionEntry(['entry_id' => 'dle_9', 'number' => 'D9', 'decision' => 'Clients may cache'])], 'markdown' => '']),
    ]);

    Livewire::test('pages::client.decision', ['project' => 'prj_cs', 'entry' => 'dle_1'])
        ->assertSet('replacements', ['dle_9' => 'D9 · Clients may cache'])
        ->set('supersededBy', 'dle_1')
        ->call('supersede')
        ->assertHasErrors('supersededBy')
        ->set('supersededBy', 'dle_9')
        ->call('supersede')
        ->assertHasNoErrors()
        ->assertSee('Superseded by')
        ->assertSee('D9 · Clients may cache')
        ->assertDontSee('Edit');

    Http::assertSent(fn (Request $request) => $request->url() === director('projects/prj_cs/decision_log/dle_1/supersede')
        && $request->data() === ['revision' => 4, 'superseded_by' => 'dle_9']);
});

it('retires an entry', function () {
    Http::fake([
        ...decisionProject(),
        director('projects/prj_cs/decision_log/dle_1/retire') => Http::response(['entry' => decisionEntry(['status' => 'retired', 'revision' => 5]), 'revisions' => [], 'markdown' => '']),
        director('projects/prj_cs/decision_log/dle_1') => Http::response(['entry' => decisionEntry(), 'revisions' => [], 'markdown' => '']),
        director('projects/prj_cs/decision_log?*') => Http::response(['entries' => [], 'markdown' => '']),
    ]);

    Livewire::test('pages::client.decision', ['project' => 'prj_cs', 'entry' => 'dle_1'])
        ->call('retire')
        ->assertSee('retired');

    Http::assertSent(fn (Request $request) => $request->url() === director('projects/prj_cs/decision_log/dle_1/retire') && $request->data() === ['revision' => 4]);
});

it('attaches the project\'s decision log to a new task lane', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(['models' => [['name' => 'codex', 'label' => 'Codex', 'available' => true]], 'default_model' => 'codex']),
        director('repos') => Http::response(['repos' => [['name' => 'contextstack']]]),
        director('projects') => Http::response(['projects' => [['project_id' => 'prj_cs', 'name' => 'ContextStack']]]),
        director('intents') => Http::response(['intent' => ['intent_id' => 'int_1'], 'jobs' => [['job_id' => 'job_1']], 'runs' => []], 201),
    ]);

    Livewire::withQueryParams(['project' => 'prj_cs'])->test('pages::client.lane-create')
        ->assertSet('projectId', 'prj_cs')
        ->set('title', 'Fix it')->set('goal', 'Make it work.')->set('lane', 'fix-it')
        ->set('decisionLog', true)
        ->call('submit')
        ->assertRedirect(route('lanes.show', 'job_1'));

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['project_id'] === 'prj_cs'
        && $request['lane']['context'] === ['decision_log']);
});

it('sends no context when the decision log is not attached, and no project when none is chosen', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(['models' => [['name' => 'codex', 'label' => 'Codex', 'available' => true]], 'default_model' => 'codex']),
        director('repos') => Http::response(['repos' => [['name' => 'contextstack']]]),
        director('projects') => Http::response(['projects' => [['project_id' => 'prj_cs', 'name' => 'ContextStack']]]),
        director('intents') => Http::response(['intent' => ['intent_id' => 'int_1'], 'jobs' => [['job_id' => 'job_1']], 'runs' => []], 201),
    ]);

    Livewire::test('pages::client.lane-create')
        ->set('title', 'Fix it')->set('goal', 'Make it work.')->set('lane', 'fix-it')
        ->set('decisionLog', true)
        ->call('submit');

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && ! isset($request['project_id'])
        && ! isset($request['lane']['context']));
});
