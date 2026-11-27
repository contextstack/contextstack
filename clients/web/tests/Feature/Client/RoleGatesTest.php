<?php

use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

it('shows starting work only to roles the Director lets start work', function (string $role, bool $shown) {
    meOnDirector(meAs($role));
    Http::fake([director('intents?*') => Http::response(['intents' => [], 'jobs' => []])]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.intents');
    $shown ? $page->assertSee('New task') : $page->assertDontSee('New task');

    $create = Livewire::test('pages::client.intent-create');
    $shown ? $create->assertDontSeeHtml('data-test="cannot-start"') : $create->assertSeeHtml('data-test="cannot-start"');
})->with([['owner', true], ['admin', true], ['member', true], ['viewer', false]]);

it('shows backlog editing to owners, admins and members, not viewers', function (string $role, bool $shown) {
    meOnDirector(meAs($role));
    Http::fake([
        director('projects/prj_1/backlog/needs_you') => Http::response(['items' => []]),
        director('projects/prj_1/backlog/next_unblocked') => Http::response(['item' => null]),
        director('projects/prj_1/backlog*') => Http::response(['items' => []]),
        director('projects/prj_1') => Http::response(['project' => ['project_id' => 'prj_1', 'name' => 'Widgets', 'revision' => 1]]),
    ]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.backlog', ['project' => 'prj_1']);
    $shown ? $page->assertSeeHtml('data-test="new-backlog-item"') : $page->assertDontSeeHtml('data-test="new-backlog-item"');
})->with([['owner', true], ['admin', true], ['member', true], ['viewer', false]]);

it('shows project creation only to owners and admins', function (string $role, bool $shown) {
    meOnDirector(meAs($role));
    Http::fake([director('projects') => Http::response(['projects' => []])]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.projects');
    $shown ? $page->assertSee('New project') : $page->assertDontSee('New project');
})->with([['owner', true], ['admin', true], ['member', false], ['viewer', false]]);

it('shows the decide controls on an action approval only to the owner or the admin it is assigned to', function (string $role, string $assignee, bool $shown) {
    meOnDirector(meAs($role));
    Http::fake([director('action-approvals') => Http::response(['action_approvals' => [[
        'id' => 'apr_1', 'job_id' => 'job_1', 'status' => 'needs_owner', 'argv' => ['git', 'push'], 'assignee_member_id' => $assignee,
        'created_at' => '2026-11-27T10:00:00Z', 'revision' => 3,
    ]]])]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.approvals');
    $shown ? $page->assertSeeHtml('data-test="approve-action"') : $page->assertDontSeeHtml('data-test="approve-action"');
})->with([
    'owner takes over' => ['owner', 'mem_other', true],
    'assigned admin' => ['admin', 'mem_me', true],
    'another admin\'s' => ['admin', 'mem_other', false],
    'member' => ['member', 'mem_me', false],
    'viewer' => ['viewer', 'mem_me', false],
]);

it('keeps every control for a Director from before teams', function () {
    Http::fake([director('action-approvals') => Http::response(['action_approvals' => [[
        'id' => 'apr_1', 'job_id' => 'job_1', 'status' => 'needs_owner', 'argv' => ['git', 'push'], 'created_at' => '2026-11-27T10:00:00Z', 'revision' => 3,
    ]]])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.approvals')->assertSeeHtml('data-test="approve-action"');
});

it('shows source budgets read-only to an admin and writable to an owner', function (string $role, bool $writes) {
    meOnDirector(meAs($role));
    Http::fake([director('source-budgets') => Http::response(['sources' => [['source' => 'openai', 'state' => 'active', 'budget' => ['usd' => 5, 'period' => 'day'], 'spent' => ['day_usd' => 1, 'month_usd' => 2], 'shown' => 'active']]])]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.sources');
    $writes ? $page->assertSeeHtml('data-test="set-source"') : $page->assertDontSeeHtml('data-test="set-source"');
})->with([['owner', true], ['admin', false]]);

it('hands a schedule to another member with a keyed PATCH, for an owner or admin', function () {
    meOnDirector(meAs('admin'));
    $schedule = ['schedule_key' => 'laptop/nightly', 'node' => 'laptop', 'schedule_id' => 'nightly', 'enabled' => true, 'cron' => '0 2 * * *', 'timezone' => 'UTC',
        'responsible_member_id' => 'mem_sam', 'removed_at' => null, 'revision' => 7];
    Http::fake([
        director('schedules/laptop/nightly') => Http::response(['schedule' => [...$schedule, 'responsible_member_id' => 'mem_kim', 'revision' => 8], 'firings' => [], 'changes' => []]),
        director('schedules') => Http::response(['schedules' => [$schedule]]),
        director('members') => Http::response(['members' => [
            ['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'member', 'status' => 'disabled'],
            ['id' => 'mem_kim', 'display_name' => 'Kim', 'role' => 'member', 'status' => 'active'],
        ]]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.schedules')
        ->assertSee('Sam')
        ->assertSeeHtml('data-test="schedule-paused"')
        ->set('assign.laptop/nightly', 'mem_kim')
        ->call('reassign', 'laptop/nightly')
        ->assertSee('Kim');

    Http::assertSent(fn (Request $request) => $request->method() === 'PATCH' && $request->url() === director('schedules/laptop/nightly')
        && $request->data() === ['responsible_member_id' => 'mem_kim'] && $request->hasHeader('Idempotency-Key'));
});

it('shows schedules to a member without reassignment', function () {
    meOnDirector(meAs('member'));
    Http::fake([director('schedules') => Http::response(['schedules' => [['schedule_key' => 'laptop/nightly', 'node' => 'laptop', 'schedule_id' => 'nightly',
        'enabled' => true, 'cron' => '0 2 * * *', 'responsible_member_id' => 'mem_me', 'revision' => 7]]])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.schedules')->assertSee('you')->assertDontSeeHtml('data-test="schedule-reassign"');

    Http::assertNotSent(fn (Request $request) => $request->url() === director('members'));
});

it('saves the owner\'s peer-message allowlist as one full list, each direction separate, and turns it off with an empty list', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('peer-message-allowlist') => Http::sequence()
            ->push(['pairs' => []])
            ->push(['pairs' => [['from_conversation' => 'con_a', 'to_conversation' => 'con_b'], ['from_conversation' => 'con_b', 'to_conversation' => 'con_a']]])
            ->push(['pairs' => []]),
        director('conversations?*') => Http::response(['conversations' => [
            ['conversation_id' => 'con_a', 'title' => 'Alpha', 'archived_at' => null],
            ['conversation_id' => 'con_b', 'title' => 'Beta', 'archived_at' => null],
            ['conversation_id' => 'con_old', 'title' => 'Old', 'archived_at' => '2026-11-01T00:00:00Z'],
        ]]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.peer-messages')
        ->assertSeeHtml('data-test="peer-off"')
        ->assertDontSee('Old')
        ->set('from', 'con_a')->set('to', 'con_b')->set('direction', 'both')
        ->call('add')
        ->assertSet('pairs', [['from_conversation' => 'con_a', 'to_conversation' => 'con_b'], ['from_conversation' => 'con_b', 'to_conversation' => 'con_a']])
        ->assertSeeHtml('data-test="peer-pair"')
        ->call('turnOff')
        ->assertSeeHtml('data-test="peer-off"');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->data() === ['pairs' => [
        ['from_conversation' => 'con_a', 'to_conversation' => 'con_b'], ['from_conversation' => 'con_b', 'to_conversation' => 'con_a'],
    ]]);
    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->data() === ['pairs' => []]);
});

it('keeps the peer-message allowlist to the owner', function () {
    meOnDirector(meAs('admin'));
    Http::fake();
    fakeDirectorBackground();

    Livewire::test('pages::client.peer-messages')->assertSeeHtml('data-test="peer-not-allowed"');

    Http::assertNotSent(fn (Request $request) => str_contains($request->url(), 'peer-message-allowlist'));
});

it('refreshes a page and who this device is when the Director refuses after a downgrade (403)', function () {
    meOnDirector(meAs('member'), meAs('viewer'));
    Http::fake([
        director('intents?*') => Http::sequence()
            ->push(['intents' => [], 'jobs' => []])
            ->push(['error' => 'Forbidden', 'code' => 'forbidden', 'detail' => 'not permitted'], 403)
            ->push(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.intents')->assertSee('New task');
    $page->call('refetch')->assertSee('not permitted')->assertDontSee('New task');
});

it('clears what a page shows when the device is revoked while it is open (401)', function () {
    Http::fake([
        director('intents?*') => Http::sequence()
            ->push(['intents' => [['intent_id' => 'int_1', 'title' => 'Fix the widget', 'node' => 'laptop', 'task' => 'tasks/fix', 'created_at' => '2026-11-27T10:00:00Z', 'revision' => 2]], 'jobs' => []])
            ->push(['error' => 'Unauthorized', 'code' => 'unauthenticated', 'detail' => 'revoked'], 401),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.intents')
        ->assertSee('Fix the widget')
        ->call('refetch')
        ->assertSet('records', [])
        ->assertSet('signedOut', true)
        ->assertDispatched('cs-signed-out')
        ->assertDontSee('Fix the widget');
});

it('shows a pair naming an archived or deleted conversation as stale and leaves it out of the next save', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('peer-message-allowlist') => Http::sequence()
            ->push(['pairs' => [
                ['from_conversation' => 'con_a', 'to_conversation' => 'con_old'],
                ['from_conversation' => 'con_a', 'to_conversation' => 'con_far'],
                ['from_conversation' => 'con_gone', 'to_conversation' => 'con_a'],
            ]])
            ->push(['pairs' => [['from_conversation' => 'con_a', 'to_conversation' => 'con_far'], ['from_conversation' => 'con_a', 'to_conversation' => 'con_b']]]),
        director('conversations?*') => Http::response(['conversations' => [
            ['conversation_id' => 'con_a', 'title' => 'Alpha', 'archived_at' => null],
            ['conversation_id' => 'con_b', 'title' => 'Beta', 'archived_at' => null],
        ]]),
        // Beyond the 500 the list holds: looked up one by one before being called stale.
        director('conversations/con_far') => Http::response(['conversation' => ['conversation_id' => 'con_far', 'title' => 'Far', 'archived_at' => null]]),
        director('conversations/con_old') => Http::response(['conversation' => ['conversation_id' => 'con_old', 'title' => 'Old', 'archived_at' => '2026-11-01T00:00:00Z']]),
        director('conversations/con_gone') => Http::response(['error' => 'Not Found', 'code' => 'not_found', 'detail' => 'gone'], 404),
    ]);

    Livewire::test('pages::client.peer-messages')
        ->assertSet('stale', ['con_gone', 'con_old'])
        ->assertSeeHtml('data-test="peer-pair-stale"')
        ->assertSee('Far')
        ->set('from', 'con_a')->set('to', 'con_b')
        ->call('add')
        ->assertSet('stale', []);

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->data() === ['pairs' => [
        ['from_conversation' => 'con_a', 'to_conversation' => 'con_far'], ['from_conversation' => 'con_a', 'to_conversation' => 'con_b'],
    ]]);
});

it('does not read a page twice when the read itself was refused', function () {
    Http::fake([director('intents?*') => Http::response(['error' => 'Forbidden', 'code' => 'forbidden', 'detail' => 'not permitted'], 403)]);

    Livewire::test('pages::client.intents')->assertSee('not permitted')->call('refetch');

    expect(Http::recorded(fn (Request $request) => str_contains($request->url(), '/intents')))->toHaveCount(2);
});
