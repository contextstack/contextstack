<?php

use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * A backlog item as the Director shows it (CLIENT-API.md "Project backlog and heartbeat").
 *
 * @return array<string, mixed>
 */
function backlogItem(array $over = []): array
{
    return [...[
        'item_id' => 'bli_1', 'project_id' => 'prj_1', 'title' => 'Ship the iPad app', 'body' => 'Pair and approve from the iPad.',
        'status' => 'next', 'priority' => 10, 'blocked_on' => [], 'blocked_reason' => '', 'stop' => false,
        'intent_ids' => [], 'job_ids' => [], 'decision_log_entry_ids' => [], 'revision' => 100,
        'created_at' => '2026-11-21T10:00:00Z', 'created_by' => 'owner', 'updated_at' => '2026-11-21T10:00:00Z', 'updated_by' => 'owner',
    ], ...$over];
}

/**
 * @param  list<array<string, mixed>>  $items
 * @return array<string, mixed>
 */
function backlogDirector(array $items, array $needsYou = [], ?array $next = null): array
{
    return [
        director('projects/prj_1') => Http::response(['project' => ['project_id' => 'prj_1', 'name' => 'ContextStack']]),
        director('projects/prj_1/backlog/needs_you') => Http::response(['items' => $needsYou]),
        director('projects/prj_1/backlog/next_unblocked') => Http::response(['item' => $next]),
    ];
}

it('lists open items in priority order with what waits on you and what is next', function () {
    $owner = backlogItem(['item_id' => 'bli_2', 'title' => 'Pick the Apple team', 'status' => 'blocked', 'priority' => 5,
        'blocked_on' => [['kind' => 'owner', 'ref' => 'owner']], 'blocked_reason' => 'Needs your Apple team ID.', 'stop' => true]);
    Http::fake([
        ...backlogDirector([], [$owner], backlogItem()),
        director('projects/prj_1/backlog*') => Http::response(['items' => [
            backlogItem(), $owner, backlogItem(['item_id' => 'bli_3', 'title' => 'Old idea', 'status' => 'dropped', 'priority' => 1]),
        ]]),
    ]);

    Livewire::test('pages::client.backlog', ['project' => 'prj_1'])
        ->assertSeeInOrder(['Waiting on you', 'Pick the Apple team', 'heartbeat paused', 'Needs your Apple team ID.'])
        ->assertSeeInOrder(['Next up:', 'Ship the iPad app'])
        ->assertSeeInOrder(['Pick the Apple team', 'blocked', 'Ship the iPad app'])
        ->assertDontSee('Old idea');
});

it('asks the Director for one status when filtered', function () {
    Http::fake([...backlogDirector([]), director('projects/prj_1/backlog*') => Http::response(['items' => [backlogItem(['status' => 'done', 'title' => 'Finished'])]])]);

    Livewire::test('pages::client.backlog', ['project' => 'prj_1'])->set('status', 'done')->assertSee('Finished');

    Http::assertSent(fn (Request $request) => str_contains($request->url(), 'backlog?status=done'));
});

it('reorders by drag with one atomic reorder against the revisions shown', function () {
    $a = backlogItem(['item_id' => 'bli_a', 'title' => 'A', 'priority' => 10, 'revision' => 1]);
    $b = backlogItem(['item_id' => 'bli_b', 'title' => 'B', 'priority' => 20, 'revision' => 2]);
    $c = backlogItem(['item_id' => 'bli_c', 'title' => 'C', 'priority' => 30, 'revision' => 3]);
    Http::fake([
        ...backlogDirector([]),
        director('projects/prj_1/backlog/reorder') => Http::response(['items' => [
            [...$c, 'priority' => 10, 'revision' => 4], [...$a, 'priority' => 20, 'revision' => 5], [...$b, 'priority' => 30, 'revision' => 6],
        ]]),
        director('projects/prj_1/backlog*') => Http::response(['items' => [$a, $b, $c]]),
    ]);

    Livewire::test('pages::client.backlog', ['project' => 'prj_1'])
        ->call('reorder', 'bli_c', 0)
        ->assertSeeInOrder(['C', 'A', 'B']);

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'backlog/reorder') && $request->hasHeader('Idempotency-Key')
        && $request['items'] === [
            ['item_id' => 'bli_c', 'revision' => 3, 'priority' => 10],
            ['item_id' => 'bli_a', 'revision' => 1, 'priority' => 20],
            ['item_id' => 'bli_b', 'revision' => 2, 'priority' => 30],
        ]);
});

it('fetches again and says so when a reorder is stale', function () {
    Http::fake([
        ...backlogDirector([]),
        director('projects/prj_1/backlog/reorder') => Http::response(['error' => 'Conflict', 'code' => 'revision_conflict', 'detail' => 'stale'], 409),
        director('projects/prj_1/backlog*') => Http::response(['items' => [backlogItem(['item_id' => 'bli_a']), backlogItem(['item_id' => 'bli_b', 'priority' => 20])]]),
    ]);

    Livewire::test('pages::client.backlog', ['project' => 'prj_1'])
        ->call('reorder', 'bli_b', 0)
        ->assertSee('The backlog changed since you opened it');
});

it('creates an item under one key', function () {
    Http::fake([
        ...backlogDirector([]),
        director('projects/prj_1/backlog') => Http::sequence()
            ->push(['items' => []])
            ->push(['item' => backlogItem(['title' => 'Try Android'])], 201)
            ->push(['items' => [backlogItem(['title' => 'Try Android'])]]),
    ]);

    Livewire::test('pages::client.backlog', ['project' => 'prj_1'])
        ->call('newItem')
        ->set('form.title', 'Try Android')
        ->call('addBlocker')
        ->set('form.blockers.0.kind', 'external')
        ->set('form.blockers.0.ref', 'Apple review')
        ->call('create')
        ->assertHasNoErrors()
        ->assertSee('Try Android');

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && str_ends_with($request->url(), 'projects/prj_1/backlog')
        && $request['title'] === 'Try Android' && $request['blocked_on'] === [['kind' => 'external', 'ref' => 'Apple review']]
        && $request['priority'] === 10 && $request->hasHeader('Idempotency-Key'));
});

it('shows an item with its blocker, links and history', function () {
    $item = backlogItem(['blocked_on' => [['kind' => 'owner', 'ref' => 'owner'], ['kind' => 'job', 'ref' => 'job_9']],
        'blocked_reason' => 'Which Apple team?', 'job_ids' => ['job_7'], 'revision' => 102, 'updated_by' => 'lead']);
    Http::fake([
        director('projects/prj_1') => Http::response(['project' => ['project_id' => 'prj_1', 'name' => 'ContextStack']]),
        director('projects/prj_1/backlog/bli_1') => Http::response(['item' => $item, 'revisions' => [
            backlogItem(['revision' => 100]), backlogItem(['revision' => 101, 'status' => 'blocked']),
        ]]),
    ]);

    Livewire::test('pages::client.backlog-item', ['project' => 'prj_1', 'item' => 'bli_1'])
        ->assertSee('This waits on you')
        ->assertSee('Which Apple team?')
        ->assertSee('job_9')
        ->assertSee('job_7')
        ->assertSeeInOrder(['v3', 'changed status, blocked on, blocked reason, job', 'by lead', 'v2', 'changed status', 'v1', 'created']);
});

it('clears the owner blocker and sends the item back to next', function () {
    $item = backlogItem(['status' => 'blocked', 'blocked_on' => [['kind' => 'owner', 'ref' => 'owner']], 'revision' => 102]);
    Http::fake([
        director('projects/prj_1') => Http::response(['project' => ['project_id' => 'prj_1', 'name' => 'ContextStack']]),
        director('projects/prj_1/backlog/bli_1/update') => Http::response(['item' => [...$item, 'status' => 'next', 'blocked_on' => [], 'revision' => 103]]),
        director('projects/prj_1/backlog/bli_1') => Http::response(['item' => $item, 'revisions' => []]),
    ]);

    Livewire::test('pages::client.backlog-item', ['project' => 'prj_1', 'item' => 'bli_1'])
        ->call('clearOwnerBlocker')
        ->assertDontSee('This waits on you');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'bli_1/update')
        && $request['blocked_on'] === [] && $request['status'] === 'next' && $request['revision'] === 102);
});

it('saves only the changed fields against the revision shown', function () {
    $item = backlogItem(['revision' => 100]);
    Http::fake([
        director('projects/prj_1') => Http::response(['project' => ['project_id' => 'prj_1', 'name' => 'ContextStack']]),
        director('projects/prj_1/backlog/bli_1/update') => Http::response(['item' => [...$item, 'title' => 'Ship the iPad app first', 'revision' => 101]]),
        director('projects/prj_1/backlog/bli_1') => Http::response(['item' => $item, 'revisions' => []]),
    ]);

    Livewire::test('pages::client.backlog-item', ['project' => 'prj_1', 'item' => 'bli_1'])
        ->call('edit')
        ->set('form.title', 'Ship the iPad app first')
        ->call('save')
        ->assertSet('editing', false)
        ->assertSee('Ship the iPad app first');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'bli_1/update') && $request->data() === ['title' => 'Ship the iPad app first', 'revision' => 100]);
});

it('shows the item as it now is when an edit is stale', function () {
    Http::fake([
        director('projects/prj_1') => Http::response(['project' => ['project_id' => 'prj_1', 'name' => 'ContextStack']]),
        director('projects/prj_1/backlog/bli_1/update') => Http::response(['error' => 'Conflict', 'code' => 'revision_conflict', 'detail' => 'stale'], 409),
        director('projects/prj_1/backlog/bli_1') => Http::sequence()
            ->push(['item' => backlogItem(), 'revisions' => []])
            ->push(['item' => backlogItem(['title' => 'Renamed by the lead', 'revision' => 101]), 'revisions' => [backlogItem()]]),
    ]);

    Livewire::test('pages::client.backlog-item', ['project' => 'prj_1', 'item' => 'bli_1'])
        ->call('setStatus', 'done')
        ->assertSee('This item changed since you opened it')
        ->assertSee('Renamed by the lead');
});

it('turns the project heartbeat on and off', function () {
    Http::fake([
        director('projects/prj_1/heartbeat') => Http::sequence()
            ->push(['project_id' => 'prj_1', 'interval_seconds' => null, 'conversation_id' => null])
            ->push(['project_id' => 'prj_1', 'interval_seconds' => 1800, 'conversation_id' => 'conv_lead'])
            ->push(['project_id' => 'prj_1', 'interval_seconds' => null, 'conversation_id' => null]),
        director('projects/prj_1') => Http::response(['project' => ['project_id' => 'prj_1', 'name' => 'ContextStack', 'budget' => ['state' => 'none']]]),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
        director('conversations*') => Http::response(['conversations' => [
            ['conversation_id' => 'conv_lead', 'project_id' => 'prj_1', 'title' => 'Release lead', 'lead' => true, 'archived_at' => null],
            ['conversation_id' => 'conv_chat', 'project_id' => 'prj_1', 'title' => 'Chat', 'lead' => false, 'archived_at' => null],
        ]]),
        director('usage*') => Http::response([]),
    ]);

    Livewire::test('pages::client.project', ['project' => 'prj_1'])
        ->assertSee('Off.')
        ->set('heartbeatMinutes', 30)
        ->call('saveHeartbeat')
        ->assertSee('On: every 30 min, in Release lead.')
        ->call('stopHeartbeat')
        ->assertSee('Off.');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request['interval_seconds'] === 1800 && $request['conversation_id'] === 'conv_lead');
    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->data() === ['interval_seconds' => null]);
});
