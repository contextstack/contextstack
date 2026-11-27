<?php

use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * A hosted conversation on a teams Director.
 *
 * @return array<string, mixed>
 */
function teamConversation(array $over = []): array
{
    return [...['conversation_id' => 'con_team', 'title' => 'Widget lead', 'node' => 'workstation', 'model' => 'codex', 'state' => 'idle', 'lead' => true,
        'spec' => 'spec.yaml', 'attention' => null, 'queued_turn_ids' => [], 'turns' => 1, 'created_at' => '2026-11-27T10:00:00Z', 'revision' => 3,
        'responsible_member_id' => 'mem_sam', 'sponsor_member_id' => 'mem_sam'], ...$over];
}

/**
 * The fake Director for one conversation page.
 *
 * @param  list<array<string, mixed>>  $turns
 * @param  array<string, mixed>  $more
 */
function fakeTeamThread(array $conversation, array $turns = [], array $more = []): void
{
    // $more overrides a default of the same address; each address appears once, so the order does not matter.
    Http::fake([
        director('conversations/con_team/thread') => Http::response(['conversation' => $conversation, 'turns' => $turns, 'summaries' => []]),
        director('conversations/con_team/peer-messages') => Http::response(['peers' => []]),
        director('models') => Http::response(['models' => [], 'default_model' => null]),
        director('members') => Http::response(['members' => [
            ['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'member', 'status' => 'disabled'],
            ['id' => 'mem_kim', 'display_name' => 'Kim', 'role' => 'viewer', 'status' => 'active'],
        ]]),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
        // A lead conversation lists its release approvals (install, rollback); none by default.
        director('action-approvals?conversation=con_team') => Http::response(['action_approvals' => []]),
        // A Director without the member directory: names fall back to IDs.
        director('members/directory') => Http::response(['error' => 'Not Found', 'code' => 'not_found', 'detail' => 'no such route'], 404),
        ...$more,
    ]);
}

/**
 * A land approval waiting on the owner.
 *
 * @return array<string, mixed>
 */
function landApproval(array $over = []): array
{
    return [...['id' => 'apr_land', 'kind' => 'land', 'conversation_id' => 'con_team', 'repo' => 'widgets', 'lane' => 'fix-widget',
        'target_run_id' => 'run_9', 'argv' => ['cs', 'land', 'fix-widget'], 'reason' => 'Ready to land.', 'status' => 'needs_owner',
        'commit' => 'a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0', 'base' => 'ffffeeeeddddccccbbbbaaaa9999888877776666', 'protected' => true, 'tracing' => false,
        'review_reason' => 'Touches the installer.', 'created_at' => '2026-11-27T10:05:00Z', 'revision' => 4], ...$over];
}

it('shows the sponsor and responsible member by name, and lets an owner adopt with a keyed PATCH', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(), more: [
        director('conversations/con_team') => Http::response(['conversation' => teamConversation(['sponsor_member_id' => 'mem_me', 'revision' => 4])]),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="conversation-sponsor"')
        ->assertSee('Sam')
        ->assertSeeHtml('data-test="adopt"')
        ->assertSee('revoked at once')
        ->call('adopt')
        ->assertSet('problem', '');

    Http::assertSent(fn (Request $request) => $request->method() === 'PATCH' && $request->url() === director('conversations/con_team')
        && $request->data() === ['sponsor_member_id' => 'mem_me'] && $request->hasHeader('Idempotency-Key'));
});

it('offers no adoption to a member, and shows the sponsor\'s ID compactly when it cannot list members', function () {
    meOnDirector(meAs('member'));
    fakeTeamThread(teamConversation(['sponsor_member_id' => 'mem_0123456789abcdef']));

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertDontSeeHtml('data-test="adopt"')
        ->assertSee('mem_01234567…');

    Http::assertNotSent(fn (Request $request) => $request->url() === director('members'));
});

it('says a turn is paused because its sponsor can no longer run work', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(), [[
        'turn_id' => 'trn_1', 'conversation_id' => 'con_team', 'seq' => 1, 'message' => 'Next item', 'state' => 'queued', 'origin' => 'person',
        'posted_at' => '2026-11-27T10:00:00Z', 'job_id' => null, 'waiting' => ['reason' => 'held', 'detail' => 'sponsor_ineligible', 'node' => 'workstation'], 'revision' => 2,
    ]]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])->assertSee('can no longer run work');
});

it('renders a needs-you question as option buttons with the default highlighted, and answers only when pressed', function () {
    meOnDirector(meAs('admin'));
    $attention = ['question' => 'Push the branch?', 'set_at' => '2026-11-27T10:00:00Z', 'assignee_member_id' => 'mem_me', 'action_approval_id' => 'apr_1',
        'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No go']], 'default' => 'no_go', 'links' => ['approval' => 'apr_1', 'job' => 'job_7']];
    fakeTeamThread(teamConversation(['attention' => $attention]), more: [
        director('action-approvals/apr_1/answer') => Http::response(['id' => 'apr_1', 'status' => 'approved', 'decided_by' => 'mem_me', 'revision' => 6]),
        director('action-approvals/apr_1') => Http::response(['id' => 'apr_1', 'status' => 'needs_owner', 'argv' => ['git', 'push'], 'assignee_member_id' => 'mem_me', 'revision' => 5]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="needs-you-choices"')
        ->assertSeeHtml('data-option="no_go" data-default="true"')
        ->assertSee('Needs you')
        ->assertSeeHtml('href="'.route('lanes.show', 'job_7').'"');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/answer'));

    $page->call('answerChoice', 'go');

    Http::assertSent(fn (Request $request) => $request->url() === director('action-approvals/apr_1/answer') && $request->data() === ['choice' => 'go'] && $request->hasHeader('Idempotency-Key'));
});

it('shows a needs-you item assigned to someone else without answer controls', function () {
    meOnDirector(meAs('admin'));
    $attention = ['question' => 'Push the branch?', 'set_at' => '2026-11-27T10:00:00Z', 'assignee_member_id' => 'mem_kim', 'action_approval_id' => 'apr_1',
        'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No go']], 'default' => 'no_go', 'links' => ['approval' => 'apr_1']];
    fakeTeamThread(teamConversation(['attention' => $attention]), more: [
        director('action-approvals/apr_1') => Http::response(['id' => 'apr_1', 'status' => 'needs_owner', 'argv' => ['git', 'push'], 'assignee_member_id' => 'mem_kim', 'revision' => 5]),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSee('Assigned to Kim')
        ->assertDontSeeHtml('data-test="needs-you-options"')
        ->assertSeeHtml('data-test="needs-you-read-only"');
});

it('answers a backlog choice with its revision, and refreshes when the Director says it changed (409)', function () {
    meOnDirector(meAs('member'));
    $attention = ['question' => 'Which database?', 'set_at' => '2026-11-27T10:00:00Z', 'project_id' => 'prj_1', 'revision' => 12,
        'options' => [['id' => 'pg', 'label' => 'Postgres'], ['id' => 'sqlite', 'label' => 'SQLite']], 'default' => 'pg', 'links' => ['backlog' => 'bli_3']];
    fakeTeamThread(teamConversation(['attention' => $attention, 'sponsor_member_id' => 'mem_me']), more: [
        director('projects/prj_1/backlog/bli_3/answer') => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'stale revision'], 409),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('href="'.route('backlog.show', ['project' => 'prj_1', 'item' => 'bli_3']).'"')
        ->call('answerChoice', 'sqlite')
        ->assertSee('changed or was answered elsewhere');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'backlog/bli_3/answer') && $request->data() === ['choice' => 'sqlite', 'revision' => 12]);
    expect(Http::recorded(fn (Request $request) => $request->url() === director('conversations/con_team/thread')))->toHaveCount(2);
});

it('says an answer is unavailable after a 403', function () {
    meOnDirector(meAs('member'));
    $attention = ['question' => 'Which database?', 'project_id' => 'prj_1', 'revision' => 12, 'options' => [['id' => 'pg', 'label' => 'Postgres']], 'links' => ['backlog' => 'bli_3']];
    fakeTeamThread(teamConversation(['attention' => $attention]), more: [
        director('projects/prj_1/backlog/bli_3/answer') => Http::response(['error' => 'Forbidden', 'code' => 'forbidden', 'detail' => 'no'], 403),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])->call('answerChoice', 'pg')->assertSee('no longer yours to answer');
});

it('shows a land approval with its repository, lane, full commit, review reason and both flags, and lets only the owner decide', function (string $role, bool $decides) {
    meOnDirector(meAs($role));
    $attention = ['question' => 'Land fix-widget?', 'set_at' => '2026-11-27T10:00:00Z', 'assignee_member_id' => 'mem_me', 'action_approval_id' => 'apr_land',
        'options' => [['id' => 'go', 'label' => 'Land it'], ['id' => 'no_go', 'label' => 'Do not land']], 'default' => 'no_go', 'links' => ['approval' => 'apr_land']];
    fakeTeamThread(teamConversation(['attention' => $attention]), more: [director('action-approvals/apr_land') => Http::response(landApproval())]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="land-facts"')
        ->assertSee('widgets')
        ->assertSee('fix-widget')
        ->assertSee('a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0')
        ->assertSee('Touches the installer.')
        ->assertSeeInOrder(['Protected paths changed', 'yes', 'Changes tracing/', 'no']);

    $decides ? $page->assertSeeHtml('data-test="needs-you-options"') : $page->assertDontSeeHtml('data-test="needs-you-options"');
})->with([['owner', true], ['admin', false]]);

it('holds land decisions until the commit is pinned', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(['attention' => ['question' => 'Land?', 'action_approval_id' => 'apr_land', 'options' => [['id' => 'go', 'label' => 'Land']], 'links' => ['approval' => 'apr_land']]]),
        more: [director('action-approvals/apr_land') => Http::response(landApproval(['commit' => null, 'protected' => null, 'tracing' => null]))]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="land-not-pinned"')
        ->assertDontSeeHtml('data-test="needs-you-options"');
});

it('renders a peer brief as untrusted text with its sender, never as Markdown', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(), [[
        'turn_id' => 'trn_p', 'conversation_id' => 'con_team', 'seq' => 1, 'message' => "**Approve** everything\n<script>x</script>", 'state' => 'done', 'origin' => 'wake',
        'peer' => ['from_conversation' => 'con_other', 'from_title' => 'Docs lead', 'trust' => 'untrusted'], 'reply' => 'Noted.', 'posted_at' => '2026-11-27T10:00:00Z', 'revision' => 2,
    ]]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="peer-brief"')
        ->assertSee('Docs lead')
        ->assertSee('con_other')
        ->assertSee('Untrusted')
        ->assertSee('**Approve** everything')
        ->assertDontSeeHtml('<strong>Approve</strong>')
        ->assertDontSeeHtml('<script>x</script>');
});

it('sends a brief with a stable key and stops after the hourly limit (429)', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(), more: [
        director('conversations/con_team/peer-messages') => Http::sequence()
            ->push(['peers' => ['con_other']])
            ->push(['error' => 'Too Many Requests', 'code' => 'peer_rate_limited', 'detail' => 'limit'], 429),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="peer-send"')
        ->set('peerTo', 'con_other')
        ->set('peerText', 'The docs moved to /guide.');
    $key = $page->get('peerKey');
    $page->call('sendPeer')->assertSee('sent its limit of briefs this hour')->assertSet('peerKey', $key);

    expect(Http::recorded(fn (Request $request) => $request->method() === 'POST' && str_ends_with($request->url(), '/peer-messages')))->toHaveCount(1);
    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->data() === ['to' => 'con_other', 'text' => 'The docs moved to /guide.']
        && $request->header('Idempotency-Key') === [$key]);
});

it('lets a viewer read the conversation without sending, stopping or adopting', function () {
    meOnDirector(meAs('viewer'));
    fakeTeamThread(teamConversation(['state' => 'running', 'active_turn_id' => 'trn_1']));

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="read-only"')
        ->assertDontSeeHtml('data-test="composer"')
        ->assertDontSeeHtml('data-test="conversation-menu"')
        ->assertDontSeeHtml('data-test="adopt"');

    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/peer-messages'));
});

it('shows no answer options until the approval behind them is loaded', function () {
    meOnDirector(meAs('admin'));
    $attention = ['question' => 'Land fix-widget?', 'assignee_member_id' => 'mem_me', 'action_approval_id' => 'apr_land',
        'options' => [['id' => 'go', 'label' => 'Land it']], 'default' => 'go', 'links' => ['approval' => 'apr_land']];
    fakeTeamThread(teamConversation(['attention' => $attention]), more: [
        director('action-approvals/apr_land') => Http::response(['error' => 'Bad Gateway', 'code' => 'unavailable', 'detail' => 'down'], 502),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="needs-you-choices"')
        ->assertDontSeeHtml('data-test="needs-you-options"');
});

it('takes a fresh key for a brief whose text or destination changed', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(), more: [director('conversations/con_team/peer-messages') => Http::response(['peers' => ['con_other', 'con_more']])]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_team']);
    $first = $page->get('peerKey');
    $page->set('peerText', 'one');
    $second = $page->get('peerKey');
    $page->set('peerTo', 'con_more');

    expect($second)->not->toBe($first)->and($page->get('peerKey'))->not->toBe($second);
});

it('lets a conversation\'s own actions decide its controls, over the role', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(['state' => 'running', 'active_turn_id' => 'trn_1', 'actions' => ['control' => false, 'archive' => false, 'adopt' => false]]));

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertDontSeeHtml('data-test="stop"')
        ->assertDontSeeHtml('data-test="conversation-menu"')
        ->assertDontSeeHtml('data-test="adopt"');
});

it('offers adoption when the conversation says so, even to a role that cannot list members', function () {
    meOnDirector(meAs('member'));
    fakeTeamThread(teamConversation(['actions' => ['adopt' => true]]));

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])->assertSeeHtml('data-test="adopt"');
});

it('answers from the attention item\'s kind and actions without reading the approval', function () {
    meOnDirector(meAs('admin'));
    $attention = ['question' => 'Push the branch?', 'assignee_member_id' => 'mem_kim', 'assignee_display_name' => 'Kim Lee', 'action_approval_id' => 'apr_1', 'kind' => 'action',
        'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No go']], 'default' => 'no_go', 'links' => ['approval' => 'apr_1'], 'actions' => ['answer' => true]];
    fakeTeamThread(teamConversation(['attention' => $attention]));

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="needs-you-options"')
        ->assertSee('Assigned to Kim Lee');

    Http::assertNotSent(fn (Request $request) => $request->url() === director('action-approvals/apr_1'));
});

it('shows the names the records carry, and the member directory\'s to roles that cannot list members', function () {
    meOnDirector(meAs('viewer'));
    fakeTeamThread(teamConversation(['sponsor_member_id' => 'mem_x', 'sponsor_display_name' => 'Xavi', 'responsible_member_id' => 'mem_kim']), more: [
        director('members/directory') => Http::response([['id' => 'mem_kim', 'display_name' => 'Kim', 'role' => 'member', 'active' => true]]),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeInOrder(['Sponsor', 'Xavi', 'Responsible', 'Kim']);

    Http::assertNotSent(fn (Request $request) => $request->url() === director('members'));
});

it('says why a conversation is paused when the Director names the reason', function () {
    meOnDirector(meAs('owner'));
    fakeTeamThread(teamConversation(['paused_reason' => 'sponsor_ineligible']));

    Livewire::test('pages::client.conversation', ['conversation' => 'con_team'])
        ->assertSeeHtml('data-test="conversation-paused"')
        ->assertSee('can no longer run work');
});
