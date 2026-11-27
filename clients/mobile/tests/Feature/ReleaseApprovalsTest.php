<?php

use App\NativeComponents\Waiting;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Native\Mobile\Events\Biometric\Completed as BiometricCompleted;
use Native\Mobile\Testing\Native;

beforeEach(function () {
    isolateDevice();
});

const ReleaseCommit = 'c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00';
const ReleaseHash = 'abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789';

/**
 * Pair as an owner (or another role) of a Director with teams.
 */
function pairReleaseAs(bool $owner = true): void
{
    Http::fake([DirectorUrl.'/api/v1/client/me' => Http::response(['credential_id' => 'cred_ipad', 'owner' => $owner,
        'member' => ['id' => 'mem_me', 'display_name' => 'Sam'], 'role' => $owner ? 'owner' : 'admin',
        'capabilities' => ['read_work', 'start_work', 'control_work', 'approve_action', 'manage_members']])]);
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/members' => Http::response(['members' => []])]);
}

/**
 * An install approval the node has pinned and routed to the owner.
 *
 * @param  array<string, mixed>  $overrides
 * @return array<string, mixed>
 */
function releaseApproval(array $overrides = []): array
{
    return array_merge(['id' => 'aa_7', 'kind' => 'install', 'bundle' => 'cs-0.3.0', 'conversation_id' => 'con_1', 'status' => 'needs_owner',
        'reason' => 'A node release switch requires owner approval.', 'commit' => ReleaseCommit, 'target_hash' => ReleaseHash,
        'release_notes_diff' => "+ Hosted installs\n- Old note", 'commit_provenance' => 'claimed commit (from bundle.json)',
        'backup' => 'Local backup only', 'verdict' => 'no_go', 'review_reason' => 'The node pinned the release. Approve the switch?',
        'outcome' => ['status' => 'waiting_on_owner'], 'created_at' => '2026-11-27T10:00:00Z', 'assignee_member_id' => 'mem_me',
        'actions' => ['decide' => true, 'answer' => true, 'clear' => true]], $overrides);
}

/**
 * @param  list<array<string, mixed>>  $needsOwner
 * @param  list<array<string, mixed>>  $conversations
 */
function fakeReleaseInbox(array $needsOwner, array $conversations = []): void
{
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals?*' => Http::response(['action_approvals' => $needsOwner]),
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => $conversations]),
    ]);
}

it('shows a pinned install with its full commit, hash, schema, notes and what approving covers', function () {
    pairReleaseAs();
    fakeReleaseInbox([releaseApproval()]);

    Native::test(Waiting::class)
        ->assertSee('Install bundle cs-0.3.0')
        ->assertSee('Release approval')
        ->call('select', 'aa_7')
        ->assertSee(ReleaseCommit)
        ->assertSee(ReleaseHash)
        ->assertSee('Current: No local Director · Target: No local Director')
        ->assertSee('+ Hosted installs')
        ->assertSee('- Old note')
        ->assertSee('also approves restoring the previous release automatically if the smoke trial fails')
        ->assertSee('Approve')
        ->assertDontSee('command is malformed');
});

it('shows the inspection state of a local-main install and offers no decision before the pin', function () {
    pairReleaseAs();
    $pending = releaseApproval(['status' => 'pending', 'commit' => null, 'target_hash' => null, 'release_notes_diff' => null, 'outcome' => null]);
    unset($pending['bundle']);
    fakeReleaseInbox([]);
    Http::fake([DirectorUrl.'/api/v1/client/action-approvals/aa_7' => Http::response($pending)]);

    Native::test(Waiting::class, data: ['approval' => 'aa_7'])
        ->assertSee('local main')
        ->assertSee('The node is inspecting')
        ->assertDontSee('Approve')
        ->assertDontSee('Deny');
});

it('shows numeric schema versions when the machine serves a local Director', function () {
    pairReleaseAs();
    fakeReleaseInbox([releaseApproval(['schema_current' => 13, 'schema_target' => 14])]);

    Native::test(Waiting::class)->call('select', 'aa_7')->assertSee('Current: 13 · Target: 14');
});

it('approves an install only after the device unlock, then follows it to the smoke result', function () {
    pairReleaseAs();
    $state = ['record' => releaseApproval()];
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals/aa_7/decide' => function () use (&$state) {
            $state['record'] = releaseApproval(['status' => 'approved', 'outcome' => ['status' => 'unknown'], 'claimed_at' => '2026-11-27T10:05:00Z']);

            return Http::response($state['record']);
        },
        DirectorUrl.'/api/v1/client/action-approvals/aa_7' => function () use (&$state) {
            return Http::response($state['record']);
        },
        DirectorUrl.'/api/v1/client/action-approvals?*' => function () use (&$state) {
            return Http::response(['action_approvals' => $state['record']['status'] === 'needs_owner' ? [$state['record']] : []]);
        },
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
        DirectorUrl.'/api/v1/client/conversations/con_1/smoke' => function () use (&$state) {
            return Http::response(['last_smoke' => ($state['record']['outcome']['smoke_ok'] ?? null) !== null ? $state['record']['outcome'] : null]);
        },
    ]);

    $screen = Native::test(Waiting::class)->call('select', 'aa_7')->tap('Approve')->assertNativeCalled('Biometric.Prompt');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/decide'));

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-decide'])
        ->assertSet('selected', 'aa_7')
        ->assertSee('The node is switching releases and running the smoke trial');
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/aa_7/decide') && $request['decision'] === 'approve');

    $state['record'] = releaseApproval(['status' => 'approved', 'claimed_at' => '2026-11-27T10:05:00Z', 'outcome' => ['status' => 'installed',
        'reason' => 'Installation smoke passed', 'release' => 'cs-0.3.0', 'smoke_ok' => true,
        'smoke' => ['ok' => true, 'checks' => [['name' => 'director', 'ok' => true], ['name' => 'gateway', 'ok' => true]]]]]);
    $screen->call('refresh')
        ->assertSee('Installed. The smoke trial passed.')
        ->assertSee('Current release: cs-0.3.0')
        ->assertSee('Smoke passed')
        ->assertSee('gateway: passed')
        ->assertSee('Latest smoke on this machine');
});

it('sends nothing when the device unlock fails', function () {
    pairReleaseAs();
    fakeReleaseInbox([releaseApproval()]);
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => false]);

    Native::test(Waiting::class)->call('select', 'aa_7')->tap('Deny')
        ->emitNative(BiometricCompleted::class, ['success' => false, 'id' => 'release-decide'])
        ->assertSee('Not unlocked, so nothing was sent.');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/decide'));
});

it('offers neither a decision nor clear on a release to an admin', function () {
    pairReleaseAs(owner: false);
    fakeReleaseInbox([releaseApproval(['actions' => null])]);

    Native::test(Waiting::class)->call('select', 'aa_7')
        ->assertSee('deciding it is not yours on this Director')
        ->assertDontSee('Clear stuck request')
        ->call('decide', 'aa_7', 'approve')
        ->assertNativeNotCalled('Biometric.Prompt');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/decide'));
});

it('clears a stuck request after a warning and the device unlock', function () {
    pairReleaseAs();
    $stuck = releaseApproval(['status' => 'approved', 'claimed_at' => '2026-11-27T10:05:00Z',
        'outcome' => ['status' => 'interrupted', 'reason' => 'Release worker lease expired. Verify the live release and smoke status on the node.']]);
    fakeReleaseInbox([]);
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals/aa_7/clear' => Http::response(array_merge($stuck, ['outcome' => $stuck['outcome'] + ['cleared' => true]])),
        DirectorUrl.'/api/v1/client/action-approvals/aa_7' => Http::response($stuck),
        DirectorUrl.'/api/v1/client/conversations/con_1/smoke' => Http::response(['last_smoke' => null]),
    ]);

    $screen = Native::test(Waiting::class, data: ['approval' => 'aa_7'])
        ->assertSee('Interrupted.')
        ->assertSee('Release worker lease expired')
        ->tap('Clear stuck request')
        ->assertSee('may still finish, so check the live release on the node before asking for another switch')
        ->tap('Clear it')
        ->assertNativeCalled('Biometric.Prompt');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/clear'));

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-clear'])
        ->assertSee('Cleared. Check the live release on the node');
    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && str_ends_with($request->url(), '/aa_7/clear'));
});

it('says when a request can no longer be cleared', function () {
    pairReleaseAs();
    fakeReleaseInbox([releaseApproval(['status' => 'pending', 'commit' => null, 'target_hash' => null])]);
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);
    Http::fake([DirectorUrl.'/api/v1/client/action-approvals/aa_7/clear' => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'changed'], 409)]);

    Native::test(Waiting::class)->call('select', 'aa_7')->tap('Clear stuck request')->tap('Clear it')
        ->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-clear'])
        ->assertSee('It can no longer be cleared');
});

it('lists a rollback waiting on jobs and acknowledges it from its approval', function () {
    pairReleaseAs();
    $failed = releaseApproval(['status' => 'approved', 'claimed_at' => '2026-11-27T10:05:00Z', 'outcome' => ['status' => 'rollback_failed',
        'reason' => 'failed release is live; rollback waiting on 3 jobs', 'release' => 'cs-0.3.0', 'smoke_ok' => false, 'waiting_jobs' => 3,
        'smoke' => ['ok' => false, 'checks' => [['name' => 'director', 'ok' => false, 'reason' => 'no answer']]]]]);
    fakeReleaseInbox([], [['conversation_id' => 'con_1', 'title' => 'Release', 'archived_at' => null, 'attention' => [
        'action_approval_id' => 'aa_7', 'kind' => 'install', 'question' => 'failed release is live; rollback waiting on 3 jobs',
        'assignee_member_id' => 'mem_me', 'options' => [['id' => 'acknowledge', 'label' => 'Acknowledge rollback is waiting']],
        'default' => null, 'links' => ['approval' => 'aa_7'], 'actions' => ['decide' => false, 'answer' => true, 'clear' => true]]]]);
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals/aa_7/answer' => Http::response(array_merge($failed, ['outcome' => $failed['outcome'] + ['acknowledged' => true]])),
        DirectorUrl.'/api/v1/client/action-approvals/aa_7' => Http::response($failed),
        DirectorUrl.'/api/v1/client/conversations/con_1/smoke' => Http::response(['last_smoke' => $failed['outcome']]),
    ]);

    Native::test(Waiting::class)
        ->assertSee('Rollback waiting · Release')
        ->assertSee('rollback waiting on 3 jobs')
        ->call('openApproval', 'aa_7')
        ->assertSee('the failed release is live, waiting on 3 jobs.')
        ->assertSee('director: failed (no answer)')
        ->assertSee('Clear stuck request')
        ->tap('Acknowledge rollback is waiting');
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/aa_7/answer') && $request['choice'] === 'acknowledge');
});

it('answers a release approval in the conversation only after the device unlock', function () {
    pairReleaseAs();
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals/aa_7/answer' => Http::response(releaseApproval(['status' => 'approved'])),
        DirectorUrl.'/api/v1/client/conversations/con_1/thread' => Http::response(['conversation' => ['conversation_id' => 'con_1', 'title' => 'Release',
            'lead' => true, 'state' => 'idle', 'revision' => 5, 'attention' => ['action_approval_id' => 'aa_7', 'kind' => 'install',
                'question' => 'The node pinned the release. Approve the switch?', 'assignee_member_id' => 'mem_me',
                'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No-go']], 'default' => 'no_go',
                'links' => ['approval' => 'aa_7'], 'actions' => ['decide' => true, 'answer' => true, 'clear' => true]]],
            'turns' => [], 'summaries' => []]),
        DirectorUrl.'/api/v1/client/conversations/con_1' => Http::response(['conversation' => ['conversation_id' => 'con_1', 'revision' => 5]]),
    ]);

    $screen = Native::visit('/conversations/con_1')
        ->assertSee('Release install approval')
        ->assertSee('Go also approves restoring the previous release')
        ->tap('Go')
        ->assertNativeCalled('Biometric.Prompt');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/answer'));

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-answer']);
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/aa_7/answer') && $request['choice'] === 'go');
});
