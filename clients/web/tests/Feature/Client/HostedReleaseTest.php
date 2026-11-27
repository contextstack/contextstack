<?php

use App\Client\Identity;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * A hosted lead conversation.
 *
 * @return array<string, mixed>
 */
function releaseConversation(array $over = []): array
{
    return [...['conversation_id' => 'con_rel', 'title' => 'Release lead', 'node' => 'workstation', 'model' => 'codex', 'state' => 'idle', 'lead' => true,
        'spec' => 'spec.yaml', 'attention' => null, 'queued_turn_ids' => [], 'turns' => 1, 'created_at' => '2026-11-27T10:00:00Z', 'revision' => 3], ...$over];
}

/**
 * An install approval (CLIENT-API.md "Hosted release approvals") pinned by the node and waiting on the owner.
 *
 * @return array<string, mixed>
 */
function installApproval(array $over = []): array
{
    return [...['id' => 'act_inst', 'kind' => 'install', 'conversation_id' => 'con_rel', 'bundle' => null, 'status' => 'needs_owner',
        'reason' => 'A node release switch requires owner approval.', 'created_at' => '2026-11-27T10:05:00Z', 'revision' => 4,
        'commit' => str_repeat('a1', 20), 'target_hash' => str_repeat('b2', 32), 'schema_current' => 14, 'schema_target' => 15,
        'release_notes_diff' => "+ Teams step 2\n- Old note <b>x</b>", 'commit_provenance' => 'Pinned from local main and built by node-bundle',
        'backup' => 'Local backup only', 'review_reason' => 'The node pinned the release. Approve the switch?',
        'outcome' => ['status' => 'waiting_on_owner']], ...$over];
}

/**
 * The fake Director for the release conversation page; $more wins over the defaults.
 *
 * @param  list<array<string, mixed>>  $approvals  what `GET action-approvals` lists
 * @param  list<array<string, mixed>>  $turns
 * @param  array<string, mixed>  $more
 */
function fakeReleaseThread(array $conversation, array $approvals = [], array $turns = [], array $more = []): void
{
    Http::fake([
        ...$more,
        director('conversations/con_rel/thread') => Http::response(['conversation' => $conversation, 'turns' => $turns, 'summaries' => []]),
        director('conversations/con_rel/peer-messages') => Http::response(['peers' => []]),
        director('action-approvals?conversation=con_rel') => Http::response(['action_approvals' => $approvals]),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();
}

it('shows an install the node is still inspecting, with its bundle and no decision yet', function () {
    meOnDirector(meAs('owner'));
    fakeReleaseThread(releaseConversation(), [['id' => 'act_inst', 'kind' => 'install', 'conversation_id' => 'con_rel', 'bundle' => 'cs-0.2.0-rc.1',
        'status' => 'pending', 'reason' => 'A node release switch requires owner approval.', 'created_at' => '2026-11-27T10:05:00Z', 'revision' => 1]]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-phase="inspecting"')
        ->assertSee('cs-0.2.0-rc.1')
        ->assertSee('Node inspecting')
        ->assertSeeHtml('data-test="release-inspecting"')
        ->assertDontSeeHtml('data-test="approve-release"');
});

it('says local main when an install names no bundle', function () {
    meOnDirector(meAs('owner'));
    fakeReleaseThread(releaseConversation(), [installApproval(['status' => 'pending', 'commit' => null, 'target_hash' => null, 'outcome' => null])]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSee('The node\'s local main');
});

it('shows a pinned install with the full commit, target hash, schemas and notes diff as text, and lets the owner approve', function () {
    meOnDirector(meAs('owner'));
    $attention = ['question' => 'The node pinned the release. Approve the switch?', 'action_approval_id' => 'act_inst', 'kind' => 'install',
        'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No go']], 'default' => 'no_go', 'links' => ['approval' => 'act_inst'],
        'actions' => ['decide' => true, 'answer' => true, 'clear' => true]];
    fakeReleaseThread(releaseConversation(['attention' => $attention]), more: [
        director('action-approvals/act_inst/decide') => Http::response(installApproval(['status' => 'approved', 'decided_by' => 'cred_this', 'outcome' => null])),
        director('action-approvals/act_inst') => Http::response(installApproval(['actions' => ['decide' => true, 'answer' => true, 'clear' => true]])),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-phase="needs_owner"')
        ->assertSee(str_repeat('a1', 20))
        ->assertSee(str_repeat('b2', 32))
        ->assertSeeInOrder(['Director schema', '14', '→', '15'])
        ->assertSeeHtml('+ Teams step 2')
        ->assertSeeHtml('- Old note &lt;b&gt;x&lt;/b&gt;')
        ->assertSee('restores the previous release automatically')
        ->assertDontSeeHtml('data-test="needs-you-choices"')
        ->assertSeeHtml('data-test="approve-release"');

    $page->call('decideApproval', 'act_inst', 'approve')->assertSet('problem', '');

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('action-approvals/act_inst/decide')
        && $request->data() === ['decision' => 'approve'] && $request->hasHeader('Idempotency-Key'));
});

it('labels absent schema values as No local Director', function () {
    meOnDirector(meAs('owner'));
    fakeReleaseThread(releaseConversation(), [installApproval(['schema_current' => null, 'schema_target' => null])]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeInOrder(['Director schema', 'No local Director', '→', 'No local Director']);
});

it('lets the record\'s actions.decide decide the controls, and falls back to owner-only', function (string $role, ?bool $decide, bool $shown) {
    meOnDirector(meAs($role));
    $approval = installApproval($decide === null ? [] : ['actions' => ['decide' => $decide, 'answer' => $decide, 'clear' => false]]);
    fakeReleaseThread(releaseConversation(), [$approval]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_rel']);
    $shown ? $page->assertSeeHtml('data-test="approve-release"') : $page->assertDontSeeHtml('data-test="approve-release"')->assertSee('Only an owner approves a release switch.');
})->with([
    'owner, no actions' => ['owner', null, true],
    'admin, no actions' => ['admin', null, false],
    'owner, decide false' => ['owner', false, false],
    'admin, decide true' => ['admin', true, true],
]);

it('shows a waiting state after approval, then the node\'s result and its last smoke when the approval changes', function () {
    meOnDirector(meAs('owner'));
    $waiting = installApproval(['status' => 'approved', 'decided_by' => 'cred_this', 'claimed_at' => '2026-11-27T10:10:00Z', 'outcome' => ['status' => 'unknown']]);
    $outcome = ['status' => 'rolled_back', 'reason' => 'Installation smoke failed and the previous release was restored', 'release' => 'cs-0.1.9',
        'smoke_ok' => false, 'smoke' => ['ok' => false, 'checks' => [['name' => 'director-health', 'ok' => false, 'reason' => 'no answer on the port']]]];
    Http::fake([
        director('action-approvals/act_inst') => Http::response([...$waiting, 'outcome' => $outcome]),
        director('conversations/con_rel/smoke') => Http::response(['last_smoke' => $outcome]),
    ]);
    fakeReleaseThread(releaseConversation(), [$waiting]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-phase="waiting"')
        ->assertSee('Switching')
        ->assertSeeHtml('data-test="release-waiting"')
        ->assertDontSeeHtml('data-test="release-last-smoke"');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/smoke'));

    $page->call('applyStream', [['kind' => 'action_approval', 'id' => 'act_inst', 'record' => ['id' => 'act_inst', 'conversation_id' => 'con_rel', 'revision' => 6]]])
        ->assertSeeHtml('data-phase="reported"')
        ->assertSee('Smoke failed; previous release restored')
        ->assertSeeInOrder(['Current release', 'cs-0.1.9', 'Smoke', 'failed', 'Reason', 'previous release was restored'])
        ->assertSee('director-health')
        ->assertSee('no answer on the port')
        ->assertSeeHtml('data-test="release-last-smoke"');

    Http::assertSent(fn (Request $request) => $request->method() === 'GET' && $request->url() === director('conversations/con_rel/smoke'));
});

it('follows a release approval of this conversation that it did not hold yet', function () {
    meOnDirector(meAs('owner'));
    Http::fake([director('action-approvals/act_new') => Http::response(installApproval(['id' => 'act_new', 'status' => 'pending', 'commit' => null, 'target_hash' => null, 'outcome' => null]))]);
    fakeReleaseThread(releaseConversation());

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertDontSeeHtml('data-test="release-approval"')
        ->call('applyStream', [['kind' => 'action_approval', 'id' => 'act_new', 'record' => ['id' => 'act_new', 'conversation_id' => 'con_rel', 'revision' => 1]]])
        ->assertSeeHtml('data-approval="act_new"');
});

it('lists only this conversation\'s release approvals, and does not list approvals for a member who cannot approve', function () {
    meOnDirector(meAs('owner'));
    fakeReleaseThread(releaseConversation(), [installApproval(['id' => 'act_other', 'conversation_id' => 'con_else']), ['id' => 'apr_land', 'kind' => 'land', 'conversation_id' => 'con_rel', 'status' => 'needs_owner'], installApproval()]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-approval="act_inst"')
        ->assertDontSeeHtml('data-approval="act_other"')
        ->assertDontSeeHtml('data-approval="apr_land"');

    meOnDirector(meAs('member'));
    app(Identity::class)->forget();
    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])->assertDontSeeHtml('data-test="release-approval"');
    expect(Http::recorded(fn (Request $request) => $request->url() === director('action-approvals?conversation=con_rel'))->count())->toBe(1);
});

it('clears a stuck request after a confirmation that says to check the live release, with a keyed POST', function () {
    meOnDirector(meAs('owner'));
    $stuck = installApproval(['status' => 'approved', 'claimed_at' => '2026-11-27T10:10:00Z', 'outcome' => ['status' => 'unknown'],
        'actions' => ['decide' => false, 'answer' => false, 'clear' => true]]);
    $cleared = [...$stuck, 'outcome' => ['status' => 'interrupted', 'reason' => 'Release request cleared by the owner.', 'cleared' => true],
        'actions' => ['decide' => false, 'answer' => false, 'clear' => false]];
    fakeReleaseThread(releaseConversation(), [$stuck], more: [
        director('action-approvals/act_inst/clear') => Http::response($cleared),
        director('conversations/con_rel/smoke') => Http::response(['last_smoke' => null]),
        director('action-approvals/act_inst') => Http::response($cleared),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-test="clear-release"')
        ->assertSee('check the live release on the node before you ask again')
        ->call('clearApproval', 'act_inst')
        ->assertSet('problem', '')
        ->assertSee('Release request cleared by the owner.')
        ->assertSee('Interrupted')
        ->assertDontSeeHtml('data-test="clear-release"');

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('action-approvals/act_inst/clear')
        && $request->body() === '{}' && $request->hasHeader('Idempotency-Key'));
});

it('offers no clear without the capability, and explains a 409 clear', function () {
    meOnDirector(meAs('admin'));
    fakeReleaseThread(releaseConversation(), [installApproval(['status' => 'approved', 'outcome' => ['status' => 'unknown']])], more: [
        director('action-approvals/act_inst/clear') => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'a release request is already open or has changed'], 409),
    ]);
    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])->assertDontSeeHtml('data-test="clear-release"');

    meOnDirector(meAs('owner'));
    app(Identity::class)->forget();
    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-test="clear-release"')
        ->call('clearApproval', 'act_inst')
        ->assertSee('already ended or changed');
});

it('shows a timed-out rollback in needs-you with the jobs left, and acknowledges it with the answer route', function () {
    meOnDirector(meAs('owner'));
    $failed = installApproval(['status' => 'approved', 'claimed_at' => '2026-11-27T10:10:00Z',
        'outcome' => ['status' => 'rollback_failed', 'reason' => 'failed release is live; rollback waiting on 3 jobs', 'release' => 'cs-0.2.0',
            'smoke_ok' => false, 'smoke' => ['ok' => false, 'checks' => []], 'waiting_jobs' => 3],
        'actions' => ['decide' => false, 'answer' => true, 'clear' => true]]);
    $attention = ['question' => 'failed release is live; rollback waiting on 3 jobs', 'action_approval_id' => 'act_inst', 'kind' => 'install',
        'source_kind' => 'action_approval', 'options' => [['id' => 'acknowledge', 'label' => 'Acknowledge rollback is waiting']], 'default' => null,
        'actions' => ['decide' => false, 'answer' => true, 'clear' => true]];
    fakeReleaseThread(releaseConversation(['attention' => $attention]), more: [
        director('action-approvals/act_inst/answer') => Http::response([...$failed, 'outcome' => [...$failed['outcome'], 'acknowledged' => true]]),
        director('action-approvals/act_inst') => Http::response($failed),
        director('conversations/con_rel/smoke') => Http::response(['last_smoke' => $failed['outcome']]),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-test="release-needs-you"')
        ->assertSee('Smoke failed; rollback not verified')
        ->assertSeeInOrder(['Jobs left', '3 jobs still running'])
        ->assertSee('intake is frozen')
        ->assertSeeHtml('data-test="acknowledge-release"')
        ->call('acknowledgeRelease', 'act_inst')
        ->assertSet('problem', '');

    Http::assertSent(fn (Request $request) => $request->url() === director('action-approvals/act_inst/answer')
        && $request->data() === ['choice' => 'acknowledge'] && $request->hasHeader('Idempotency-Key'));
});

it('links the node\'s post-install wake turn to its approval', function () {
    meOnDirector(meAs('owner'));
    fakeReleaseThread(releaseConversation(), turns: [[
        'turn_id' => 'trn_w', 'conversation_id' => 'con_rel', 'seq' => 2, 'origin' => 'wake', 'posted_by' => 'node-install', 'state' => 'done',
        'message' => 'Post-install status for act_inst, commit abc. Installation smoke passed. Current release: cs-0.2.0.', 'reply' => 'Installed.',
        'detail' => ['approval_id' => 'act_inst'], 'posted_at' => '2026-11-27T11:00:00Z', 'revision' => 2,
    ]]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeHtml('data-test="release-wake"')
        ->assertSeeHtml('href="#release-act_inst"')
        ->assertSee('Current release: cs-0.2.0.')
        ->assertDontSeeHtml('data-test="wake-marker"');
});

it('renders a release approval on the approvals page as a release action, not a sandbox command', function () {
    meOnDirector(meAs('owner'));
    Http::fake([director('action-approvals*') => Http::response(['action_approvals' => [installApproval()]])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.approvals')
        ->assertSeeHtml('data-test="release-approval"')
        ->assertSee('Install a release on this node')
        ->assertDontSee('An agent asks to go beyond its sandbox')
        ->assertSeeHtml('href="'.route('conversations.show', 'con_rel').'"');
});

it('shows well-formed smoke checks and skips one missing its name or ok', function () {
    meOnDirector(meAs('owner'));
    $outcome = ['status' => 'installed', 'reason' => 'Installation smoke passed', 'release' => 'cs-0.2.0', 'smoke_ok' => true,
        'smoke' => ['ok' => true, 'checks' => [
            ['name' => 'director-health', 'ok' => true],
            ['name' => 'gateway-health', 'ok' => false, 'reason' => 'slow start'],
            ['name' => 'no-ok-check'],
            ['ok' => true, 'reason' => 'nameless check'],
            ['name' => 'string-ok-check', 'ok' => 'yes'],
        ]]];
    fakeReleaseThread(releaseConversation(), [installApproval(['status' => 'approved', 'claimed_at' => '2026-11-27T10:10:00Z', 'outcome' => $outcome])],
        more: [director('conversations/con_rel/smoke') => Http::response(['last_smoke' => $outcome])]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertSeeInOrder(['director-health', 'gateway-health', 'slow start'])
        ->assertSeeHtml('data-test="release-smoke-check" data-ok="false"')
        ->assertDontSee('no-ok-check')
        ->assertDontSee('nameless check')
        ->assertDontSee('string-ok-check');
});

it('does not link a node-install wake turn that carries no approval_id', function () {
    meOnDirector(meAs('owner'));
    fakeReleaseThread(releaseConversation(), turns: [[
        'turn_id' => 'trn_w', 'conversation_id' => 'con_rel', 'seq' => 2, 'origin' => 'wake', 'posted_by' => 'node-install', 'state' => 'done',
        'message' => 'Post-install status for act_inst, commit abc.', 'reply' => 'Installed.',
        'posted_at' => '2026-11-27T11:00:00Z', 'revision' => 2,
    ]]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_rel'])
        ->assertDontSeeHtml('href="#release-act_inst"');
});
