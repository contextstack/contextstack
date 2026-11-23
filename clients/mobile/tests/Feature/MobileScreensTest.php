<?php

use App\Client\ConnectionStore;
use App\Native\AppLock;
use App\NativeComponents\Connect;
use App\NativeComponents\Devices;
use App\NativeComponents\Locked;
use App\NativeComponents\Settings;
use App\NativeComponents\Waiting;
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
        ['id' => 'conv_1', 'title' => 'Release prep', 'attention' => ['question' => 'Ship rc.1 tonight?'], 'archived_at' => null],
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

it('treats a missing Keychain token as not paired', function () {
    pairDevice();
    Native::fakeBridge()->respondTo('SecureStorage.Get', fn () => ['value' => null]);

    expect(app(ConnectionStore::class)->isConnected())->toBeFalse();
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
