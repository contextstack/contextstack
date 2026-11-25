<?php

use App\Client\ConnectionStore;
use App\Native\AppLock;
use App\Native\When;
use App\NativeComponents\Connect;
use App\NativeComponents\Conversations;
use App\NativeComponents\Devices;
use App\NativeComponents\Lanes;
use App\NativeComponents\Locked;
use App\NativeComponents\NewConversation;
use App\NativeComponents\NewLane;
use App\NativeComponents\Settings;
use App\NativeComponents\Waiting;
use Carbon\CarbonImmutable;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Native\Mobile\Events\App\AppForegrounded;
use Native\Mobile\Events\Biometric\Completed as BiometricCompleted;
use Native\Mobile\Events\Scanner\CodeScanned;
use Native\Mobile\Testing\Native;

const DirectorUrl = 'http://192.0.2.1:8780';

beforeEach(function () {
    $directory = sys_get_temp_dir().'/cs-mobile-test-'.bin2hex(random_bytes(4));
    config(['client.settings_path' => $directory.'/connection.json', 'client.lock_path' => $directory.'/lock.json', 'client.director_url' => DirectorUrl]);
    Http::preventStrayRequests();
    Native::fakeBridge()->withSecureStorage();
});

function pairDevice(): void
{
    app(ConnectionStore::class)->saveDevice(DirectorUrl, 'csd_test_token', 'cred_ipad', 'iPad');
    Http::fake([DirectorUrl.'/api/v1/client/projects' => Http::response(['projects' => []])]);
}

/**
 * @param  array<string, mixed>  $overrides
 * @return array<string, mixed>
 */
function needsOwner(array $overrides = []): array
{
    return array_merge([
        'id' => 'aa_1', 'job_id' => 'job_1', 'run_id' => 'run_1', 'status' => 'needs_owner',
        'argv' => ['git', 'push', 'origin', 'main'], 'needs' => ['network'], 'write_paths' => [],
        'reason' => 'Publish the fix', 'requesting_model' => 'opus', 'reviewer_model' => 'sonnet',
        'verdict' => 'no_go', 'review_reason' => 'Git push requires an owner decision.',
        'outcome' => ['status' => 'waiting_on_owner'], 'created_at' => '2026-11-21T10:00:00Z',
    ], $overrides);
}

function fakeWaiting(array $approvals, array $conversations = []): void
{
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals?*' => Http::response(['action_approvals' => $approvals]),
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => $conversations]),
    ]);
}

it('pairs with a code and keeps only the device token', function () {
    Http::fake([DirectorUrl.'/api/v1/pair' => Http::response(['credential_id' => 'cred_ipad', 'name' => 'iPad', 'token' => 'csd_new'])]);

    Native::test(Connect::class)
        ->assertSet('url', DirectorUrl)
        ->input('code', 'ABCD-1234')
        ->tap('Pair')
        ->assertSet('problem', '')
        ->assertReplacedWith('/');

    $connection = app(ConnectionStore::class)->resolve();
    expect($connection->token)->toBe('csd_new')->and($connection->credentialId)->toBe('cred_ipad');
    Native::fakeBridge()->assertStored(ConnectionStore::TokenKey, 'csd_new');
    expect(file_get_contents(config('client.settings_path')))->not->toContain('csd_new');
    Http::assertSent(fn (Request $request) => $request->url() === DirectorUrl.'/api/v1/pair' && $request['code'] === 'ABCD-1234');
});

it('explains a refused pairing code', function () {
    Http::fake([DirectorUrl.'/api/v1/pair' => Http::response(['error' => 'Forbidden', 'code' => 'pairing_refused', 'detail' => 'no live code'], 403)]);

    Native::test(Connect::class)
        ->input('code', 'WRONG')
        ->tap('Pair')
        ->assertSee('That code was not accepted');

    expect(app(ConnectionStore::class)->isConnected())->toBeFalse();
});

it('asks for an http address before pairing', function () {
    Native::test(Connect::class)
        ->input('url', '192.0.2.1:8780')
        ->input('code', 'ABCD')
        ->tap('Pair')
        ->assertSee('including http or https at the start');

    Http::assertNothingSent();
});

it('lists approvals that need the owner and lead questions', function () {
    pairDevice();
    fakeWaiting([needsOwner()], [
        ['conversation_id' => 'conv_1', 'title' => 'Release prep', 'attention' => ['question' => 'Ship rc.1 tonight?'], 'archived_at' => null],
        ['id' => 'conv_2', 'title' => 'Quiet', 'attention' => null, 'archived_at' => null],
    ]);

    Native::test(Waiting::class)
        ->assertSee('git push origin main')
        ->assertSee('Release prep')
        ->assertSee('Ship rc.1 tonight?')
        ->assertDontSee('Quiet');

    Http::assertSent(fn (Request $request) => str_contains($request->url(), 'action-approvals?status=needs_owner')
        && $request->header('Authorization') === ['Bearer csd_test_token']);
});

it('shows the approval detail and approves it', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals/aa_1/decide' => Http::response(needsOwner(['status' => 'approved', 'decided_by' => 'cred_ipad']), 201),
        DirectorUrl.'/api/v1/client/action-approvals?*' => Http::sequence()
            ->push(['action_approvals' => [needsOwner()]])
            ->push(['action_approvals' => []]),
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
    ]);

    Native::test(Waiting::class)
        ->call('select', 'aa_1')
        ->assertSee('Git push requires an owner decision.')
        ->assertSee('No go from sonnet')
        ->tap('Approve')
        ->assertSet('selected', null)
        ->assertSee('Nothing waits on you.');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'aa_1/decide')
        && $request['decision'] === 'approve' && $request->hasHeader('Idempotency-Key'));
});

it('explains when the Director refuses this credential a decision', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals/aa_1/decide' => Http::response(['error' => 'Forbidden', 'code' => 'owner_only', 'detail' => 'only the owner may decide an action approval'], 403),
        DirectorUrl.'/api/v1/client/action-approvals?*' => Http::response(['action_approvals' => [needsOwner()]]),
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
    ]);

    Native::test(Waiting::class)
        ->call('select', 'aa_1')
        ->tap('Deny')
        ->assertSee('does not accept approval decisions from this credential');
});

it('says so when the Director cannot be reached', function () {
    pairDevice();
    Http::fake(fn () => throw new ConnectionException('timed out'));

    Native::test(Waiting::class)->assertSee('Check that Tailscale is on');
});

it('sends an unpaired device to connect', function () {
    Native::test(Waiting::class)->assertReplacedWith('/connect');
});

it('forgets the Director', function () {
    pairDevice();

    Native::test(Settings::class)
        ->assertSee('cred_ipad')
        ->tap('Forget this Director')
        ->assertReplacedWith('/connect');

    expect(app(ConnectionStore::class)->isConnected())->toBeFalse();
});

it('refreshes and explains when the approval was already decided', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals/aa_1/decide' => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'already decided'], 409),
        DirectorUrl.'/api/v1/client/action-approvals?*' => Http::sequence()
            ->push(['action_approvals' => [needsOwner()]])
            ->push(['action_approvals' => []]),
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
    ]);

    Native::test(Waiting::class)
        ->call('select', 'aa_1')
        ->tap('Approve')
        ->assertSet('selected', null)
        ->assertSee('already decided, or it no longer waits on you');
});

it('moves a token an earlier build left in the file into the Keychain', function () {
    $path = config('client.settings_path');
    mkdir(dirname($path), 0700, true);
    file_put_contents($path, json_encode(['url' => DirectorUrl, 'token' => 'csd_old', 'credential_id' => 'cred_ipad', 'name' => 'ipad']));

    expect(app(ConnectionStore::class)->resolve()->token)->toBe('csd_old');
    Native::fakeBridge()->assertStored(ConnectionStore::TokenKey, 'csd_old');
    expect(file_get_contents($path))->not->toContain('csd_old')->toContain('cred_ipad');
    expect(app(ConnectionStore::class)->resolve()->token)->toBe('csd_old');
});

it('keeps a paired device paired when the Keychain does not give its token, and says so instead of asking to pair again', function () {
    pairDevice();
    Native::fakeBridge()->respondTo('SecureStorage.Get', fn () => ['value' => null]);

    expect(app(ConnectionStore::class)->isConnected())->toBeTrue();
    Native::visit('/projects')->assertSee('could not be read from the Keychain just now')->assertNoNavigation();
});

it('pairs at once from a scanned pairing code', function () {
    Http::fake([DirectorUrl.'/api/v1/pair' => Http::response(['credential_id' => 'cred_ipad', 'name' => 'ipad', 'token' => 'csd_scanned'])]);

    Native::test(Connect::class)
        ->emitNative(CodeScanned::class, ['data' => 'contextstack://pair?url='.urlencode(DirectorUrl).'&code=K7QM-2XPA', 'format' => 'qr'])
        ->assertReplacedWith('/');

    Http::assertSent(fn (Request $request) => $request['code'] === 'K7QM-2XPA');
    expect(app(ConnectionStore::class)->resolve()->token)->toBe('csd_scanned');
});

it('refuses a scanned code that is not a pairing link', function () {
    Native::test(Connect::class)
        ->emitNative(CodeScanned::class, ['data' => 'https://example.com/pair?code=1', 'format' => 'qr'])
        ->assertSee('That is not a ContextStack pairing code');

    Http::assertNothingSent();
});

it('fills the address and code from a pairing link and waits for Pair', function () {
    Native::visit('/pair?url='.urlencode('http://192.0.2.9:8780').'&code=ABCD-EFGH')
        ->assertSet('url', 'http://192.0.2.9:8780')
        ->assertSet('code', 'ABCD-EFGH');

    Http::assertNothingSent();
});

function fakeCredentials(): void
{
    Http::fake([DirectorUrl.'/api/v1/client/credentials' => Http::response(['credentials' => [
        ['id' => 'cred_ipad', 'kind' => 'device', 'name' => 'ipad', 'created_at' => '2026-11-21T10:00:00Z', 'revoked_at' => null],
        ['id' => 'cred_phone', 'kind' => 'device', 'name' => 'phone', 'created_at' => '2026-11-21T11:00:00Z', 'revoked_at' => null],
        ['id' => 'cred_local', 'kind' => 'local', 'name' => 'local', 'created_at' => '2026-11-01T00:00:00Z'],
    ]])]);
}

it('lists paired devices and marks this one', function () {
    pairDevice();
    fakeCredentials();

    Native::test(Devices::class)->assertSee('phone')->assertSee('This device')->assertDontSee('local');
});

it('pairs another device only after the device unlock succeeds', function () {
    pairDevice();
    fakeCredentials();
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);
    Http::fake([DirectorUrl.'/api/v1/client/pairings' => Http::response(['code' => 'QQQQ-1111', 'name' => 'phone2', 'expires_at' => '2026-11-21T12:10:00Z'])]);

    $screen = Native::test(Devices::class)->input('name', 'phone2')->tap('Get a pairing code');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/pairings'));

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'pair'])
        ->assertSee('QQQQ-1111')
        ->assertSee(DirectorUrl);
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/pairings') && $request['name'] === 'phone2');
});

it('changes nothing when the device unlock fails', function () {
    pairDevice();
    fakeCredentials();
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);

    Native::test(Devices::class)
        ->call('revoke', 'cred_phone')
        ->emitNative(BiometricCompleted::class, ['success' => false, 'id' => 'revoke:cred_phone'])
        ->assertSee('Not unlocked, so nothing changed.');

    Http::assertNotSent(fn (Request $request) => $request->method() === 'DELETE');
});

it('revokes another device after the unlock, never itself', function () {
    pairDevice();
    fakeCredentials();
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);
    Http::fake([DirectorUrl.'/api/v1/client/credentials/cred_phone' => Http::response(['id' => 'cred_phone', 'revoked' => true])]);

    Native::test(Devices::class)
        ->call('revoke', 'cred_ipad')
        ->call('revoke', 'cred_phone')
        ->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'revoke:cred_phone']);

    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && str_ends_with($request->url(), 'credentials/cred_phone'));
    Http::assertNotSent(fn (Request $request) => $request->method() === 'DELETE' && str_ends_with($request->url(), 'credentials/cred_ipad'));
});

it('explains when the Director refuses this credential device management', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/credentials' => Http::response(['error' => 'Forbidden', 'code' => 'owner_only', 'detail' => 'owner only'], 403)]);

    Native::test(Devices::class)->assertSee('does not let this credential manage devices');
});

function lockApp(int $afterSeconds = 60): AppLock
{
    $lock = app(AppLock::class);
    $lock->setAfterSeconds($afterSeconds);
    $lock->enable();
    app()->forgetInstance(AppLock::class);

    return app(AppLock::class);
}

it('turns the app lock on only after the device unlock succeeds', function () {
    pairDevice();
    Native::fakeBridge()->respondTo('Biometric.Prompt', fn () => ['success' => true]);

    $screen = Native::test(Settings::class)->tap('Turn on app lock');
    expect(app(AppLock::class)->enabled())->toBeFalse();

    $screen->emitNative(BiometricCompleted::class, ['success' => false, 'id' => 'lock-on'])
        ->assertSee('Not unlocked, so the app lock did not change.');
    expect(app(AppLock::class)->enabled())->toBeFalse();

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'lock-on'])
        ->assertSet('lockEnabled', true)
        ->assertSee('After 1 minute');
    expect(app(AppLock::class)->enabled())->toBeTrue()->and(app(AppLock::class)->locked())->toBeFalse();
});

it('turns the app lock off only after the device unlock succeeds', function () {
    pairDevice();
    lockApp()->unlock();

    Native::test(Settings::class)
        ->tap('Turn off app lock')
        ->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'pair'])
        ->assertSet('lockEnabled', true)
        ->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'lock-off'])
        ->assertSet('lockEnabled', false);

    expect(app(AppLock::class)->enabled())->toBeFalse();
});

it('keeps the chosen time away', function () {
    pairDevice();
    lockApp()->unlock();

    Native::test(Settings::class)->call('lockAfterSeconds', 300)->assertSet('lockAfter', 300)
        ->call('lockAfterSeconds', 42)->assertSet('lockAfter', 300);
});

it('starts locked on launch and shows nothing from the Director', function () {
    pairDevice();
    lockApp();

    Native::visit('/projects')->assertReplacedWith('/locked');
    Http::assertNothingSent();
    expect(app(AppLock::class)->returnTo())->toBe('/projects');
});

it('asks for the unlock and returns to the screen that was asked for', function () {
    pairDevice();
    lockApp()->stoppedAt('/projects');

    $screen = Native::visit('/locked');
    $screen->assertNativeCalled('Biometric.Prompt');

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'pair'])->assertNoNavigation();
    $screen->emitNative(BiometricCompleted::class, ['success' => false, 'id' => AppLock::PromptId])->assertSee('Not unlocked. Try again.');
    expect(app(AppLock::class)->locked())->toBeTrue();

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => AppLock::PromptId])->assertReplacedWith('/projects');
    expect(app(AppLock::class)->locked())->toBeFalse();
});

it('only returns to a screen inside the app', function () {
    pairDevice();
    lockApp()->stoppedAt('//evil.example/x');

    Native::visit('/locked')
        ->emitNative(BiometricCompleted::class, ['success' => true, 'id' => AppLock::PromptId])
        ->assertReplacedWith('/');
});

it('locks again when the app comes back after the time away, not before', function () {
    pairDevice();
    fakeWaiting([]);
    lockApp(300)->unlock();

    $screen = Native::test(Waiting::class);
    $screen->emitNative(AppForegrounded::class, ['secondsAway' => 120])->assertNoNavigation();
    expect(app(AppLock::class)->locked())->toBeFalse();

    $screen->emitNative(AppForegrounded::class, ['secondsAway' => 300])->assertReplacedWith('/locked');
    expect(app(AppLock::class)->locked())->toBeTrue();
});

it('lets a device that cannot unlock forget the Director', function () {
    pairDevice();
    lockApp();

    Native::test(Locked::class)->tap('Forget this Director')->assertReplacedWith('/connect');

    expect(app(ConnectionStore::class)->isConnected())->toBeFalse()
        ->and(app(AppLock::class)->enabled())->toBeFalse();
});

it('gives the screens opened from another screen a back button', function () {
    pairDevice();
    fakeCredentials();
    Http::fake([
        DirectorUrl.'/api/v1/client/projects/prj_1' => Http::response(['project' => ['id' => 'prj_1', 'name' => 'ContextStack']]),
        DirectorUrl.'/api/v1/client/projects/prj_1/backlog*' => Http::response(['items' => []]),
    ]);
    $back = fn (array $node): bool => ($node['props']['back'] ?? false) === true;

    Native::visit('/projects/prj_1')->assertSee('Nothing in this backlog.')->assertElement('native_root_stack', $back);
    Native::visit('/devices')->assertElement('native_root_stack', $back);
});

function conversationThread(array $turns, array $conversation = []): array
{
    return ['conversation' => array_merge(['conversation_id' => 'con_1', 'title' => 'Planning', 'lead' => true, 'model' => 'claude-code-sonnet', 'state' => 'idle', 'held' => false], $conversation), 'turns' => $turns, 'summaries' => []];
}

it('lists conversations newest first and marks the lead', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => [
        ['conversation_id' => 'con_1', 'title' => 'Planning', 'lead' => true, 'model' => 'claude-code-sonnet', 'state' => 'running', 'turns' => 3],
        ['conversation_id' => 'con_2', 'title' => '', 'lead' => false, 'model' => 'codex', 'state' => 'idle', 'turns' => 1],
    ]])]);

    Native::test(Conversations::class)
        ->assertSee('Planning')
        ->assertSee('Lead · claude-code-sonnet · replying · 3 turns')
        ->assertSee('codex · 1 turn');
});

it('shows a thread newest first with each reply or what is happening instead', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/conversations/con_1/thread' => Http::response(conversationThread([
        ['turn_id' => 'trn_1', 'seq' => 1, 'message' => 'First question', 'state' => 'done', 'reply' => 'First answer', 'posted_by_name' => 'ipad'],
        ['turn_id' => 'trn_2', 'seq' => 2, 'message' => 'Second question', 'state' => 'running', 'reply' => null],
        ['turn_id' => 'trn_3', 'seq' => 3, 'message' => 'Third', 'state' => 'error', 'detail' => 'model refused'],
    ]))]);

    $screen = Native::visit('/conversations/con_1')
        ->assertSee('First answer')->assertSee('Replying…')->assertSee('Failed. model refused');
    $seqs = array_map(fn (array $turn): int => $turn['seq'], $screen->get('turns'));
    expect($seqs)->toBe([3, 2, 1]);
});

it('posts a message with one key, clears the box and says when it is queued', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/conversations/con_1/turns' => Http::response(['turn' => ['turn_id' => 'trn_2', 'state' => 'queued'], 'conversation' => []], 201),
        DirectorUrl.'/api/v1/client/conversations/con_1/thread' => Http::response(conversationThread([])),
    ]);

    Native::visit('/conversations/con_1')
        ->set('message', '  Plan the next release  ')
        ->call('post')
        ->assertSet('message', '')
        ->assertSee('Queued behind the current reply.');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'con_1/turns')
        && $request['message'] === 'Plan the next release' && $request->hasHeader('Idempotency-Key'));
});

it('keeps the same key when the answer to a message is lost, so sending again replays it', function () {
    pairDevice();
    $keys = [];
    Http::fake(function (Request $request) use (&$keys) {
        if (str_ends_with($request->url(), '/thread')) {
            return Http::response(conversationThread([]));
        }
        $keys[] = $request->header('Idempotency-Key')[0];
        if (count($keys) === 1) {
            throw new ConnectionException('timed out');
        }

        return Http::response(['turn' => ['turn_id' => 'trn_1', 'state' => 'running'], 'conversation' => []], 201);
    });

    $screen = Native::visit('/conversations/con_1')->set('message', 'Hello')->call('post')
        ->assertSee('Sending again is safe')->assertSet('message', 'Hello');
    $screen->call('post')->assertSet('message', '');

    expect($keys)->toHaveCount(2)->and($keys[0])->toBe($keys[1]);
});

it('refuses an empty message without calling the Director', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/conversations/con_1/thread' => Http::response(conversationThread([]))]);

    Native::visit('/conversations/con_1')->set('message', '   ')->call('post')->assertSee('Write a message first.');

    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/turns'));
});

it('stops a conversation and lets a stopped one go on', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/conversations/con_1/stop' => Http::response(['conversation' => [], 'cancel' => null], 202),
        DirectorUrl.'/api/v1/client/conversations/con_1/thread' => Http::response(conversationThread([], ['state' => 'held', 'held' => true])),
        DirectorUrl.'/api/v1/client/conversations/con_1/resume' => Http::response(['conversation' => [], 'cancel' => null]),
    ]);

    Native::visit('/conversations/con_1')->call('stopConversation')->assertSee('new messages wait until you tap Go on')->tap('Go on');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'con_1/stop') && $request->hasHeader('Idempotency-Key'));
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'con_1/resume') && $request->hasHeader('Idempotency-Key'));
});

it('goes back from a conversation without stopping it', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/conversations/con_1/thread' => Http::response(conversationThread([]))]);

    Native::visit('/conversations/con_1')->pressBack()->assertWentBack();

    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/stop'));
});

function laneIntent(array $over = []): array
{
    return array_merge(['intent_id' => 'int_1', 'title' => 'Fix the widget', 'task' => 'tasks/fix-the-widget',
        'meta' => ['source' => 'client-mobile-lane', 'repo' => 'contextstack', 'lane' => 'fix-widget', 'model' => 'auto']], $over);
}

function laneJob(array $over = []): array
{
    return array_merge(['job_id' => 'job_code', 'intent_id' => 'int_1', 'node' => 'node-a', 'state' => 'running', 'created_at' => '2026-11-23T20:00:00Z',
        'meta' => ['phase' => 'coding', 'route' => ['model' => 'harness:local-coder', 'reason' => 'plan names rust → harness:local-coder']],
        'run_id' => null, 'run_ids' => [], 'cancel' => null, 'land' => null], $over);
}

function fakeLane(array $job, array $runs = [], ?array $receipt = null): void
{
    $plan = laneJob(['job_id' => 'job_plan', 'state' => 'done', 'created_at' => '2026-11-23T19:58:00Z', 'meta' => ['phase' => 'planning']]);
    Http::fake([
        DirectorUrl.'/api/v1/client/jobs/'.$job['job_id'] => Http::response($job),
        DirectorUrl.'/api/v1/client/intents/int_1' => Http::response(['intent' => laneIntent(), 'jobs' => [$plan, $job], 'runs' => $runs]),
        DirectorUrl.'/api/v1/client/runs/*/decision' => $receipt === null ? Http::response(['error' => 'Not found', 'code' => 'not_found'], 404) : Http::response($receipt),
    ]);
}

it('lists task lanes newest first, one row per lane', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/intents?*' => Http::response([
        'intents' => [laneIntent(), laneIntent(['intent_id' => 'int_2', 'title' => 'Older lane', 'meta' => ['source' => 'cs-mcp', 'repo' => 'contextstack', 'lane' => 'old']])],
        'jobs' => [
            laneJob(['job_id' => 'job_plan', 'state' => 'done', 'created_at' => '2026-11-23T19:58:00Z']),
            laneJob(),
            laneJob(['job_id' => 'job_old', 'intent_id' => 'int_2', 'state' => 'done', 'created_at' => '2026-11-20T10:00:00Z']),
        ],
    ])]);

    $screen = Native::test(Lanes::class)->assertSee('Fix the widget')->assertSee('running · contextstack/fix-widget · 2 jobs · ');
    expect(array_column($screen->get('lanes'), 'job_id'))->toBe(['job_code', 'job_old']);
});

it('shows a lane with the routed model and why, and its planning job', function () {
    pairDevice();
    fakeLane(laneJob());

    Native::visit('/lanes/job_code')
        ->assertSee('harness:local-coder')
        ->assertSee('plan names rust → harness:local-coder')
        ->assertSee('contextstack/fix-widget')
        ->assertSee('#1 planning')
        ->assertSee('Cancel this job');
});

it('cancels a running job with an idempotency key', function () {
    pairDevice();
    fakeLane(laneJob());
    Http::fake([DirectorUrl.'/api/v1/client/jobs/job_code/cancel' => Http::response(['cancel_id' => 'can_1', 'state' => 'requested', 'revision' => 1], 202)]);

    Native::visit('/lanes/job_code')->tap('Cancel this job');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'jobs/job_code/cancel') && $request->hasHeader('Idempotency-Key'));
});

it('approves a reported run and then shows how to land it', function () {
    pairDevice();
    $job = laneJob(['state' => 'done', 'run_id' => 'run_1', 'run_ids' => ['run_1']]);
    fakeLane($job, [['run_id' => 'run_1', 'job_id' => 'job_code', 'verdict' => 'pass', 'decision' => null]]);
    Http::fake([DirectorUrl.'/api/v1/client/runs/run_1/decision' => Http::response(['decision_id' => 'dec_1', 'decision' => 'approve'], 201)]);

    Native::visit('/lanes/job_code')
        ->assertSee('Approve or reject its result')
        ->assertSee('cs land contextstack fix-widget')
        ->tap('Approve');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'runs/run_1/decision') && $request->method() === 'POST'
        && $request['decision'] === 'approve' && $request->hasHeader('Idempotency-Key'));
});

it('starts a lane on Auto by default, then opens its coding job', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/nodes' => Http::response(['nodes' => [['name' => 'node-a', 'revoked_at' => null]]]),
        DirectorUrl.'/api/v1/client/repos' => Http::response(['repos' => [['name' => 'contextstack', 'node' => 'node-a', 'path' => '/src/contextstack', 'branch' => 'main']]]),
        DirectorUrl.'/api/v1/client/models' => Http::response(catalog()),
        DirectorUrl.'/api/v1/client/intents' => Http::response(['intent' => laneIntent(), 'jobs' => [laneJob(['job_id' => 'job_plan'])]], 201),
    ]);

    Native::test(NewLane::class)
        ->set('title', 'Fix the widget')
        ->set('goal', 'Make the widget stop crashing.')
        ->call('start')
        ->assertReplacedWith('/lanes/job_plan');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/client/intents') && $request->method() === 'POST'
        && $request['lane']['model'] === 'auto' && ! isset($request['lane']['plan_with'])
        && $request['lane']['repo'] === 'contextstack' && $request['lane']['lane'] === 'fix-the-widget'
        && $request['node'] === 'node-a' && $request['meta']['source'] === 'client-mobile-lane' && $request->hasHeader('Idempotency-Key'));
});

it('sends plan_with only for an explicit model', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/nodes' => Http::response(['nodes' => [['name' => 'node-a', 'revoked_at' => null]]]),
        DirectorUrl.'/api/v1/client/repos' => Http::response(['repos' => [['name' => 'contextstack', 'node' => 'node-a', 'path' => '/src/contextstack', 'branch' => 'main']]]),
        DirectorUrl.'/api/v1/client/models' => Http::response(catalog()),
        DirectorUrl.'/api/v1/client/intents' => Http::response(['intent' => laneIntent(), 'jobs' => [laneJob()]], 201),
    ]);

    Native::test(NewLane::class)
        ->set('title', 'Fix it')->set('goal', 'Fix it.')
        ->set('provider', 'OpenAI')->assertSet('model', 'GPT-5 Codex (CLI default)')->set('planFirst', 'Claude Sonnet')
        ->call('start');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/client/intents')
        && $request['lane']['model'] === 'codex' && $request['lane']['plan_with'] === 'claude-code-sonnet');
});

it('refuses to start a lane without a goal', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/nodes' => Http::response(['nodes' => [['name' => 'node-a', 'revoked_at' => null]]]),
        DirectorUrl.'/api/v1/client/repos' => Http::response(['repos' => [['name' => 'contextstack', 'node' => 'node-a', 'path' => '/src/contextstack', 'branch' => 'main']]]),
        DirectorUrl.'/api/v1/client/models' => Http::response(['models' => []]),
    ]);

    Native::test(NewLane::class)->set('title', 'Fix it')->call('start')->assertSee('Say what the lane should do.');

    Http::assertNotSent(fn (Request $request) => $request->method() === 'POST');
});

it('draws list icons, so the chosen app-lock time shows its checkmark', function () {
    pairDevice();
    lockApp(300);
    app(AppLock::class)->unlock();

    Native::test(Settings::class)
        ->assertElement('list_item', fn (array $node): bool => ($node['props']['headline'] ?? null) === 'After 5 minutes'
            && ($node['props']['leading_type'] ?? null) === 'icon' && str_contains((string) json_encode($node['props']['leading_value'] ?? ''), 'checkmark.circle.fill'));
});

it('shows lane times as how long ago', function () {
    $this->travelTo(CarbonImmutable::parse('2026-11-23T20:30:00Z'));
    pairDevice();
    fakeLane(laneJob(['state' => 'done', 'finished_at' => '2026-11-23T20:25:00Z']));

    Native::visit('/lanes/job_code')->assertSee('30 minutes ago')->assertSee('5 minutes ago');
});

it('words a missing or unreadable time plainly', function () {
    expect(When::relative(null))->toBe('—')
        ->and(When::relative('not a time'))->toBe('not a time');
});

it('starts a chat with a catalog model, sends the first message and opens it', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/models' => Http::response(catalog()),
        DirectorUrl.'/api/v1/client/conversations' => Http::response(['conversation' => ['conversation_id' => 'con_9']], 201),
        DirectorUrl.'/api/v1/client/conversations/con_9/turns' => Http::response(['turn' => ['state' => 'running'], 'conversation' => []], 201),
    ]);

    Native::test(NewConversation::class)
        ->assertSet('provider', 'OpenAI')
        ->assertSet('model', 'GPT-5 Codex (CLI default)')
        ->assertSee('Runs on node-a')
        ->set('message', 'Hello')
        ->call('start')
        ->assertReplacedWith('/conversations/con_9');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/client/conversations') && $request['spec'] === 'chat'
        && $request['model'] === 'codex' && ! isset($request['title']) && $request->hasHeader('Idempotency-Key'));
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), 'con_9/turns') && $request['message'] === 'Hello');
});

it('starts a hosted lead on a model that can lead', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/models' => Http::response(catalog()),
        DirectorUrl.'/api/v1/client/conversations' => Http::response(['conversation' => ['conversation_id' => 'con_lead', 'lead' => true]], 201),
        DirectorUrl.'/api/v1/client/conversations/con_lead/turns' => Http::response(['turn' => ['state' => 'running'], 'conversation' => []], 201),
    ]);

    Native::test(NewConversation::class)
        ->set('kind', NewConversation::Lead)
        ->assertSet('provider', 'Anthropic')
        ->assertSet('model', 'Claude Code, Sonnet 4.5')
        ->set('title', 'Evening lead')
        ->set('message', 'What needs doing?')
        ->call('start');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/client/conversations') && $request['spec'] === 'hosted-lead'
        && $request['model'] === 'claude-code-sonnet' && $request['title'] === 'Evening lead');
});

it('does not start a conversation twice when only the first message failed to arrive', function () {
    pairDevice();
    $turns = 0;
    Http::fake(function (Request $request) use (&$turns) {
        return match (true) {
            str_ends_with($request->url(), '/models') => Http::response(['models' => [['name' => 'codex', 'label' => 'Codex']]]),
            str_ends_with($request->url(), '/client/conversations') => Http::response(['conversation' => ['conversation_id' => 'con_9']], 201),
            default => ++$turns === 1 ? throw new ConnectionException('timed out') : Http::response(['turn' => ['state' => 'running'], 'conversation' => []], 201),
        };
    });

    $screen = Native::test(NewConversation::class)->set('message', 'Hello')->call('start')->assertSee('Sending again is safe');
    $screen->call('start')->assertReplacedWith('/conversations/con_9');

    expect(collect(Http::recorded())->filter(fn (array $pair): bool => str_ends_with($pair[0]->url(), '/client/conversations'))->count())->toBe(1)
        ->and($turns)->toBe(2);
});

it('lists machines with their state, open jobs and current quota limits', function () {
    $this->travelTo(CarbonImmutable::parse('2026-11-23T20:30:00Z'));
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/nodes' => Http::response(['stale_after_seconds' => 90, 'nodes' => [
        ['name' => 'node-a', 'stale' => false, 'listening' => true, 'last_seen_at' => '2026-11-23T20:29:50Z', 'open_jobs' => 2, 'revoked_at' => null,
            'status' => ['provider_limits' => [['template' => 'cursor-agent', 'resets_at' => '2026-11-30T00:00:00Z'], ['template' => 'codex', 'resets_at' => '2026-11-01T00:00:00Z']]]],
        ['name' => 'node-b', 'stale' => true, 'last_seen_at' => '2026-11-22T20:30:00Z', 'open_jobs' => 0, 'revoked_at' => null],
        ['name' => 'old', 'stale' => true, 'last_seen_at' => null, 'open_jobs' => 0, 'revoked_at' => '2026-11-10T00:00:00Z'],
    ]])]);

    Native::visit('/machines')
        ->assertSee('within 90 seconds')
        ->assertSee('listening · seen 10 seconds ago · 2 open jobs · out of quota: cursor-agent')
        ->assertSee('stale · seen 1 day ago · 0 open jobs')
        ->assertSee('revoked · seen never · 0 open jobs')
        ->assertDontSee('codex');
});

it('shows usage by source for what each is, never an estimate, and changes period', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/usage*' => Http::response(['from' => '2026-11-23', 'to' => '2026-11-24', 'groups' => [
        ['source' => ['model' => 'claude-sonnet-4-5'], 'kind' => 'pay_as_you_go', 'runs' => 3, 'charged_usd' => 1.234, 'estimated_usd' => 9.99],
        ['source' => ['model' => 'claude-code-sonnet'], 'kind' => 'subscription', 'runs' => 5, 'shown' => '40% of the weekly allowance'],
        ['source' => ['model' => 'local-coder'], 'kind' => 'local', 'runs' => 1],
    ], 'total' => ['runs' => 9, 'kinds' => ['pay_as_you_go' => ['runs' => 3, 'charged_usd' => 1.234], 'local' => ['runs' => 1]]]])]);

    $screen = Native::visit('/usage')
        ->assertSee('pay as you go · 3 runs · $1.23 charged')
        ->assertSee('subscription · 5 runs · 40% of the weekly allowance')
        ->assertSee('local · 1 run · usage only, no cost')
        ->assertSee('Pay as you go: 3 runs · $1.23 charged')
        ->assertDontSee('9.99');
    $screen->set('period', 'This month');

    Http::assertSent(fn (Request $request) => str_contains($request->url(), 'period=month') && str_contains($request->url(), 'by=source'));
});

function catalog(): array
{
    return ['default_model' => 'codex', 'models' => [
        ['name' => 'claude-code-sonnet', 'provider' => 'anthropic', 'provider_label' => 'Anthropic', 'label' => 'Claude Code, Sonnet 4.5', 'nodes' => ['node-a'], 'internal' => false, 'lead' => true, 'available' => true],
        ['name' => 'claude-code-sonnet-planner', 'provider' => 'anthropic', 'provider_label' => 'Anthropic', 'label' => 'Planner', 'nodes' => ['node-a'], 'internal' => true, 'lead' => false, 'available' => true],
        ['name' => 'claude-sonnet-4-5', 'provider' => 'anthropic', 'provider_label' => 'Anthropic', 'label' => 'Claude Sonnet 4.5 (API)', 'nodes' => [], 'internal' => false, 'lead' => false, 'available' => true],
        ['name' => 'codex', 'provider' => 'openai', 'provider_label' => 'OpenAI', 'label' => 'GPT-5 Codex (CLI default)', 'nodes' => ['node-a', 'node-b'], 'internal' => false, 'lead' => false, 'available' => true],
        ['name' => 'gone', 'provider' => 'openai', 'provider_label' => 'OpenAI', 'label' => 'Gone', 'nodes' => [], 'internal' => false, 'lead' => false, 'available' => false],
    ]];
}

it('offers models by provider, hides helper roles and unavailable ones, and says where each runs', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/models' => Http::response(catalog())]);

    $screen = Native::test(NewConversation::class)
        ->set('provider', 'Anthropic')
        ->assertSet('model', 'Claude Code, Sonnet 4.5')
        ->assertSee('Runs on node-a');
    expect($screen->instance()->providerChoices())->toBe(['Anthropic', 'OpenAI'])
        ->and($screen->instance()->modelChoices())->toBe(['Claude Code, Sonnet 4.5', 'Claude Sonnet 4.5 (API)']);

    $screen->set('model', 'Claude Sonnet 4.5 (API)')->assertSee('No machine reports it yet');
    $screen->set('provider', 'OpenAI');
    expect($screen->instance()->modelChoices())->toBe(['GPT-5 Codex (CLI default)']);
});

it('offers a lead only the models that can lead', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/models' => Http::response(catalog())]);

    $screen = Native::test(NewConversation::class)->set('kind', NewConversation::Lead);
    expect($screen->instance()->providerChoices())->toBe(['Anthropic'])
        ->and($screen->instance()->modelChoices())->toBe(['Claude Code, Sonnet 4.5']);
});

function budgetReport(array $sources = [], ?array $total = null): array
{
    return ['gateway' => '127.0.0.1:9200', 'budgets' => ['total' => $total, 'sources' => $sources ?: [
        ['source' => 'openai', 'state' => 'active', 'budget' => ['usd' => 5, 'period' => 'day'], 'shared' => false, 'spent' => ['day_usd' => 1.2, 'month_usd' => 8], 'shown' => 'active'],
        ['source' => 'anthropic', 'state' => 'off', 'budget' => null, 'shared' => false, 'spent' => ['day_usd' => 0, 'month_usd' => 0], 'shown' => 'off, set a budget to use'],
    ]]];
}

it('lists source budgets with what each has spent', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/source-budgets' => Http::response(budgetReport(total: ['usd' => 10, 'period' => 'month', 'remaining_usd' => 7.5]))]);

    Native::visit('/budgets')
        ->assertSee('$10.00 per month, $7.50 left')
        ->assertSee('active · $5.00 per day · $1.20 today, $8.00 this month')
        ->assertSee('off · no budget · $0.00 today, $0.00 this month');
});

it('sets a budget only after the device unlock succeeds', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/source-budgets' => Http::response(budgetReport()),
        DirectorUrl.'/api/v1/client/source-budgets/anthropic' => Http::response(budgetReport()),
    ]);

    $screen = Native::visit('/budgets')->call('edit', 'anthropic')->set('usd', '2.50')->set('period', 'Per month')->call('setBudget')
        ->assertNativeCalled('Biometric.Prompt');
    Http::assertNotSent(fn (Request $request) => $request->method() === 'PUT');

    $screen->emitNative(BiometricCompleted::class, ['success' => false, 'id' => 'budget-set'])->assertSee('the budget did not change');
    Http::assertNotSent(fn (Request $request) => $request->method() === 'PUT');

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'budget-set'])->assertSet('editing', '');
    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && str_ends_with($request->url(), 'source-budgets/anthropic')
        && $request['usd'] == 2.5 && $request['period'] === 'month');
});

it('turns off a budget after the unlock, and refuses an amount that is not a number', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/source-budgets' => Http::response(budgetReport()),
        DirectorUrl.'/api/v1/client/source-budgets/openai' => Http::response(budgetReport()),
    ]);

    $screen = Native::visit('/budgets')->call('edit', 'openai')->set('usd', 'lots')->call('setBudget')->assertSee('Enter a dollar amount');
    $screen->call('turnOff', 'openai')->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'budget-off']);

    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && str_ends_with($request->url(), 'source-budgets/openai'));
});

it('shows what an action needs, its job and run, and flags hidden characters in the command', function () {
    pairDevice();
    fakeWaiting([needsOwner(['argv' => ['sh', '-c', "echo ok\u{202E}gnp.exe"], 'needs' => ['network'], 'job_id' => 'job_9', 'run_id' => 'run_9'])]);

    Native::test(Waiting::class)->call('select', 'aa_1')
        ->assertSee('It needs')->assertSee('network')
        ->assertSee('Job job_9 · run run_9')
        ->assertSee('\\u{202E}')
        ->assertSee('invisible or text-reordering characters');
});

it('flags a malformed command instead of dropping the argument', function () {
    pairDevice();
    fakeWaiting([needsOwner(['argv' => ['rm', 7]])]);

    Native::test(Waiting::class)->call('select', 'aa_1')->assertSee('<not a string: int>')->assertSee('malformed');
});

it('opens the conversation a lead question came from', function () {
    pairDevice();
    fakeWaiting([], [['conversation_id' => 'con_7', 'title' => 'Release prep', 'attention' => ['question' => 'Ship rc.1 tonight?'], 'archived_at' => null]]);

    Native::test(Waiting::class)->assertSee('Ship rc.1 tonight?')->tap('Release prep')->assertNavigatedTo('/conversations/con_7');
});

it('never offers to land a rejected run or a planning job', function () {
    pairDevice();
    $job = laneJob(['state' => 'done', 'run_id' => 'run_1', 'run_ids' => ['run_1']]);
    fakeLane($job, [['run_id' => 'run_1', 'job_id' => 'job_code', 'verdict' => 'pass', 'decision' => ['decision_id' => 'dec_1']]], ['decision_id' => 'dec_1', 'decision' => 'reject']);
    Native::visit('/lanes/job_code')->assertSee('Decision: reject')->assertDontSee('cs land');

    $plan = laneJob(['job_id' => 'job_planned', 'state' => 'done', 'run_id' => 'run_p', 'run_ids' => ['run_p'], 'meta' => ['phase' => 'planning']]);
    fakeLane($plan, [['run_id' => 'run_p', 'job_id' => 'job_planned', 'verdict' => 'pass', 'decision' => null]]);
    Native::visit('/lanes/job_planned')->assertSee("lane's planning job")->assertDontSee('cs land')->assertDontSee('Approve');
});

it('offers the second cancel that finishes a job whose cancel was applied', function () {
    pairDevice();
    fakeLane(laneJob(['state' => 'accepted', 'cancel' => ['cancel_id' => 'can_1', 'state' => 'applied']]));

    Native::visit('/lanes/job_code')->assertSee('Cancel: applied')->assertSee('Cancel again to finish this job');
});

it('runs a new lane on a current machine that has the repo, and keeps the lane name following the title until edited', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/nodes' => Http::response(['nodes' => [['name' => 'node-a', 'stale' => false], ['name' => 'node-b', 'stale' => true], ['name' => 'node-c', 'stale' => false]]]),
        DirectorUrl.'/api/v1/client/repos' => Http::response(['repos' => [['name' => 'site', 'node' => 'node-b'], ['name' => 'site', 'node' => 'node-c'], ['name' => 'contextstack', 'node' => 'node-a']]]),
        DirectorUrl.'/api/v1/client/models' => Http::response(catalog()),
    ]);

    $screen = Native::test(NewLane::class)->assertSet('repo', 'contextstack')->assertSet('node', 'node-a');
    expect($screen->get('repos'))->toBe(['contextstack', 'site']);
    $screen->set('repo', 'site')->assertSet('node', 'node-c');
    expect($screen->instance()->nodeChoices())->toBe(['node-b', 'node-c']);

    $screen->set('title', 'Fi')->assertSet('lane', 'fi')->set('title', 'Fix the widget')->assertSet('lane', 'fix-the-widget')
        ->set('lane', 'my-lane')->set('title', 'Something else')->assertSet('lane', 'my-lane');
});

it('starts a fresh conversation when the kind changes after a first message failed', function () {
    pairDevice();
    $created = [];
    Http::fake(function (Request $request) use (&$created) {
        return match (true) {
            str_ends_with($request->url(), '/models') => Http::response(catalog()),
            str_ends_with($request->url(), '/client/conversations') => Http::response(['conversation' => ['conversation_id' => 'con_'.count($created = [...$created, $request['spec']])]], 201),
            str_ends_with($request->url(), 'con_1/turns') => throw new ConnectionException('timed out'),
            default => Http::response(['turn' => ['state' => 'running'], 'conversation' => []], 201),
        };
    });

    $screen = Native::test(NewConversation::class)->set('message', 'Hello')->call('start')->assertSee('Sending again is safe');
    $screen->set('kind', NewConversation::Lead)->call('start')->assertReplacedWith('/conversations/con_2');

    expect($created)->toBe(['chat', 'hosted-lead']);
});

it('does not lock on the brief return after the unlock sheet, even when set to Immediately', function () {
    $lock = lockApp(0);
    $lock->unlock();

    $lock->returned(0);
    $lock->returned(1);
    expect($lock->locked())->toBeFalse();

    $lock->returned(2);
    expect($lock->locked())->toBeTrue();
});

it('asks for the unlock before the pairing screen when the app is locked, and warns before replacing a pairing', function () {
    pairDevice();
    lockApp();

    Native::visit('/pair?url='.urlencode('http://192.0.2.50:8780').'&code=ABCD-EFGH')->assertReplacedWith('/locked');

    app(AppLock::class)->unlock();
    Native::visit('/connect')->assertSee('already paired with '.DirectorUrl);
});

it('keeps the rest of Waiting when one project\'s backlog fails', function () {
    Http::fake([
        DirectorUrl.'/api/v1/client/action-approvals?*' => Http::response(['action_approvals' => [needsOwner()]]),
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
        DirectorUrl.'/api/v1/client/projects' => Http::response(['projects' => [['project_id' => 'prj_a', 'name' => 'A'], ['project_id' => 'prj_b', 'name' => 'B']]]),
        DirectorUrl.'/api/v1/client/projects/prj_a/backlog/needs_you' => Http::response(['error' => 'Internal', 'code' => 'internal'], 500),
        DirectorUrl.'/api/v1/client/projects/prj_b/backlog/needs_you' => Http::response(['items' => [['item_id' => 'itm_1', 'title' => 'Pick a name', 'blocked_reason' => 'Your call']]]),
    ]);
    pairDevice();

    Native::test(Waiting::class)->assertSet('loaded', true)
        ->assertSee('git push origin main')->assertSee('Pick a name')->assertSee('could not be read; the rest is shown');
});

it('fetches the whole thread again only when the conversation has changed', function () {
    pairDevice();
    $threads = 0;
    $revision = 5;
    Http::fake(function (Request $request) use (&$threads, &$revision) {
        if (str_ends_with($request->url(), '/thread')) {
            $threads++;

            return Http::response(conversationThread([], ['revision' => $revision]));
        }

        return Http::response(['conversation' => ['conversation_id' => 'con_1', 'revision' => $revision]]);
    });

    $screen = Native::visit('/conversations/con_1');
    $screen->call('refresh')->call('refresh');
    expect($threads)->toBe(1);

    $revision = 6;
    $screen->call('refresh');
    expect($threads)->toBe(2);
});

it('says plainly when the owner stopped a turn', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/conversations/con_1/thread' => Http::response(conversationThread([
        ['turn_id' => 'trn_1', 'seq' => 1, 'message' => 'Capital of the moon?', 'state' => 'stopped', 'detail' => "stopped at the person's request"],
    ]))]);

    Native::visit('/conversations/con_1')->assertSee('You stopped this turn.')->assertDontSee("person's request");
});

function harnessCatalog(): array
{
    $flat = catalog();
    $flat['models'][] = ['name' => 'cursor-sonnet', 'provider' => 'cursor', 'provider_label' => 'Cursor', 'label' => 'Sonnet via Cursor', 'nodes' => ['node-a'], 'internal' => false, 'lead' => false, 'available' => true];

    return $flat + ['catalog' => [
        'providers' => [['id' => 'anthropic', 'label' => 'Anthropic'], ['id' => 'openai', 'label' => 'OpenAI']],
        'harnesses' => [
            ['id' => 'claude-code', 'label' => 'Claude Code', 'kind' => 'cli', 'provider_ids' => ['anthropic'], 'nodes' => ['node-a'], 'billing' => 'subscription', 'unavailable_reason' => null],
            ['id' => 'cursor-agent', 'label' => 'Cursor', 'kind' => 'cli', 'provider_ids' => ['anthropic', 'openai'], 'nodes' => ['node-a'], 'billing' => 'subscription', 'unavailable_reason' => null],
            ['id' => 'codex', 'label' => 'Codex CLI', 'kind' => 'cli', 'provider_ids' => ['openai'], 'nodes' => ['node-a', 'node-b'], 'billing' => 'subscription', 'unavailable_reason' => null],
            ['id' => 'gemini-cli', 'label' => 'Gemini CLI', 'kind' => 'cli', 'provider_ids' => ['google'], 'nodes' => [], 'billing' => 'subscription', 'unavailable_reason' => 'not signed in'],
        ],
        'models' => [
            ['id' => 'sonnet', 'label' => 'Claude Sonnet 5.5', 'version' => '5.5', 'provider' => 'anthropic', 'internal' => false, 'lead' => true, 'available' => true,
                'harnesses' => [['harness' => 'claude-code', 'value' => 'claude-code-sonnet'], ['harness' => 'cursor-agent', 'value' => 'cursor-sonnet']]],
            ['id' => 'planner', 'label' => 'Planner', 'version' => null, 'provider' => 'anthropic', 'internal' => true, 'lead' => false, 'available' => true,
                'harnesses' => [['harness' => 'claude-code', 'value' => 'claude-code-sonnet-planner']]],
            ['id' => 'codex-default', 'label' => "Codex: the CLI's default model", 'version' => null, 'provider' => 'openai', 'internal' => false, 'lead' => false, 'available' => true,
                'harnesses' => [['harness' => 'codex', 'value' => 'codex']]],
            ['id' => 'gemini-default', 'label' => 'Gemini CLI: its default model', 'version' => null, 'provider' => 'openai', 'internal' => false, 'lead' => false, 'available' => true,
                'harnesses' => [['harness' => 'gemini-cli', 'value' => 'gemini-cli']]],
        ],
        'default' => ['harness' => 'codex', 'model' => 'codex-default', 'value' => 'codex'],
    ]];
}

it('picks provider, then model, then the harness that runs it, and sends that pair\'s value', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/models' => Http::response(harnessCatalog()),
        DirectorUrl.'/api/v1/client/conversations' => Http::response(['conversation' => ['conversation_id' => 'con_9']], 201),
        DirectorUrl.'/api/v1/client/conversations/con_9/turns' => Http::response(['turn' => ['state' => 'running'], 'conversation' => []], 201),
    ]);

    $screen = Native::test(NewConversation::class)
        ->assertSet('provider', 'OpenAI')->assertSet('model', "Codex: the CLI's default model")->assertSet('harness', 'Codex CLI')
        ->assertSee('Runs in Codex CLI · subscription · on node-a, node-b');
    expect($screen->instance()->modelChoices())->toBe(["Codex: the CLI's default model"])
        ->and($screen->instance()->harnessChoices())->toBe(['Codex CLI']);

    $screen->set('provider', 'Anthropic')->assertSet('model', 'Claude Sonnet 5.5')->assertSet('harness', 'Claude Code');
    expect($screen->instance()->modelChoices())->toBe(['Claude Sonnet 5.5'])
        ->and($screen->instance()->harnessChoices())->toBe(['Claude Code', 'Cursor']);

    $screen->set('harness', 'Cursor')->set('message', 'Hello')->call('start');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/client/conversations') && $request['model'] === 'cursor-sonnet');
});

it('starts a lane with the chosen harness pair, and keeps Auto as the default', function () {
    pairDevice();
    Http::fake([
        DirectorUrl.'/api/v1/client/nodes' => Http::response(['nodes' => [['name' => 'node-a', 'stale' => false]]]),
        DirectorUrl.'/api/v1/client/repos' => Http::response(['repos' => [['name' => 'contextstack', 'node' => 'node-a']]]),
        DirectorUrl.'/api/v1/client/models' => Http::response(harnessCatalog()),
        DirectorUrl.'/api/v1/client/intents' => Http::response(['intent' => laneIntent(), 'jobs' => [laneJob()]], 201),
    ]);

    Native::test(NewLane::class)->assertSet('provider', NewLane::Auto)
        ->set('title', 'Fix it')->set('goal', 'Fix it.')
        ->set('provider', 'Anthropic')->set('harness', 'Cursor')
        ->call('start');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/client/intents') && $request['lane']['model'] === 'cursor-sonnet');
});

it('offers a lead only lead models from the catalog, and leaves out unavailable harnesses and helper roles', function () {
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/models' => Http::response(harnessCatalog())]);

    $screen = Native::test(NewConversation::class)->set('kind', NewConversation::Lead);
    expect($screen->instance()->providerChoices())->toBe(['Anthropic'])
        ->and($screen->instance()->modelChoices())->toBe(['Claude Sonnet 5.5'])
        ->and($screen->instance()->harnessChoices())->toBe(['Claude Code']);

    $screen->set('kind', NewConversation::Chat)->set('provider', 'OpenAI');
    expect($screen->instance()->modelChoices())->toBe(["Codex: the CLI's default model"]);
});

it('says a turn is waiting for its machine instead of replying, and keeps reading the thread while it runs', function () {
    $this->travelTo(CarbonImmutable::parse('2026-11-24T10:00:00Z'));
    pairDevice();
    $threads = 0;
    Http::fake(function (Request $request) use (&$threads) {
        if (str_ends_with($request->url(), '/thread')) {
            $threads++;

            return Http::response(conversationThread([
                ['turn_id' => 'trn_2', 'seq' => 2, 'message' => 'Held one', 'state' => 'queued', 'waiting' => ['reason' => 'held', 'detail' => 'over budget']],
                ['turn_id' => 'trn_1', 'seq' => 1, 'message' => 'Capital?', 'state' => 'running',
                    'waiting' => ['reason' => 'node_not_pulled', 'node' => 'node-a', 'since' => '2026-11-24T09:59:00Z', 'node_last_seen' => '2026-11-24T09:50:00Z']],
            ], ['revision' => 3]));
        }

        return Http::response(['conversation' => ['conversation_id' => 'con_1', 'revision' => 3]]);
    });

    $screen = Native::visit('/conversations/con_1')
        ->assertSee('Waiting for node-a to pick this up (last heard from 10 minutes ago)…')
        ->assertSee('Held: over budget.')
        ->assertDontSee('Replying…');
    $screen->call('refresh');
    expect($threads)->toBe(2);
});
