<?php

use App\Client\ConnectionStore;
use App\Client\Identity;
use App\Client\InviteLink;
use App\Http\Controllers\Client\EventStreamController;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

it('treats a me without capabilities as a Director from before teams: everything shows as before', function () {
    meOnDirector(['credential_id' => 'cred_local', 'owner' => true]);

    $identity = app(Identity::class);

    expect($identity->legacy())->toBeTrue()
        ->and($identity->can('manage_members'))->toBeTrue()
        ->and($identity->can('start_work'))->toBeTrue()
        ->and($identity->role())->toBeNull();
});

it('answers capabilities from the Director, not from the role name', function () {
    meOnDirector([...meAs('admin'), 'capabilities' => ['read_work']]);

    $identity = app(Identity::class);

    expect($identity->can('read_work'))->toBeTrue()
        ->and($identity->can('manage_members'))->toBeFalse()
        ->and($identity->role())->toBe('admin')
        ->and($identity->displayName())->toBe('Robin')
        ->and($identity->isMe('mem_me'))->toBeTrue();
});

it('keeps one me answer until it is forgotten', function () {
    meOnDirector(meAs('owner'), meAs('viewer'));
    $identity = app(Identity::class);

    expect($identity->role())->toBe('owner')->and($identity->role())->toBe('owner');
    $identity->forget();
    expect($identity->role())->toBe('viewer');
    expect(Http::recorded(fn (Request $request) => str_ends_with($request->url(), '/me')))->toHaveCount(2);
});

it('can do nothing once the Director refuses the credential', function () {
    meOnDirector([['error' => 'Unauthorized', 'code' => 'unauthenticated', 'detail' => 'x'], 401]);

    expect(app(Identity::class)->unauthenticated())->toBeTrue()
        ->and(app(Identity::class)->can('read_work'))->toBeFalse();
});

it('hides the page and offers Settings when the Director refuses this device', function () {
    meOnDirector([['error' => 'Unauthorized', 'code' => 'unauthenticated', 'detail' => 'x'], 401]);

    $this->get(route('members'))->assertOk()->assertSeeHtml('data-test="signed-out"')->assertDontSeeHtml('data-test="members-page"');
});

it('shows the Members link only to a role that manages members', function (string $role, bool $shown) {
    meOnDirector(meAs($role));
    Http::fake([
        director('credentials') => Http::response(['credentials' => []]),
    ]);
    fakeDirectorBackground();

    $page = $this->get(route('devices'))->assertOk();
    $shown ? $page->assertSeeHtml('data-test="nav-members"') : $page->assertDontSeeHtml('data-test="nav-members"');
})->with([['owner', true], ['admin', true], ['member', false], ['viewer', false]]);

it('shows the member and role on Connect', function () {
    meOnDirector(meAs('admin'));
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->assertSee('Signed in as')
        ->assertSee('Robin')
        ->assertSee('Connected as admin')
        ->assertDontSeeHtml('data-test="chief-of-staff"');
});

it('joins with an invite code and keeps the token like a pairing, never showing it', function () {
    unlink(config('client.local_credential'));
    meOnDirector(meAs('member', 'mem_alex', 'Alex'));
    Http::fake([
        'http://other.test/api/v1/invites/accept' => Http::response(['credential_id' => 'cred_new', 'name' => 'office-mac', 'token' => 'csd_invited_token',
            'member' => ['id' => 'mem_alex', 'display_name' => 'Alex'], 'role' => 'member'], 201),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->set('url', 'http://other.test')
        ->set('inviteCode', 'csi_abc')
        ->set('inviteDevice', 'office-mac')
        ->call('acceptInvite')
        ->assertHasNoErrors()
        ->assertSet('inviteCode', '')
        ->assertSee('Alex')
        ->assertSee('Connected as member')
        ->assertDontSee('csd_invited_token');

    Http::assertSent(fn (Request $request) => $request->url() === 'http://other.test/api/v1/invites/accept'
        && $request->data() === ['code' => 'csi_abc', 'name' => 'office-mac'] && ! $request->hasHeader('Authorization'));
    $connection = app(ConnectionStore::class)->resolve();
    expect($connection->token)->toBe('csd_invited_token')->and($connection->credentialId)->toBe('cred_new');
});

it('takes the Director URL and code from a pasted invite link', function () {
    meOnDirector(meAs('viewer'));
    Http::fake([
        'http://invited.test:8780/api/v1/invites/accept' => Http::response(['credential_id' => 'cred_new', 'name' => 'mac', 'token' => 'csd_t', 'member' => [], 'role' => 'viewer'], 201),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->set('inviteCode', InviteLink::make('http://invited.test:8780', 'csi_link'))
        ->set('inviteDevice', 'mac')
        ->call('acceptInvite')
        ->assertHasNoErrors()
        ->assertSet('url', 'http://invited.test:8780');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/invites/accept') && $request['code'] === 'csi_link');
});

it('says plainly when an invite is refused, and saves nothing', function () {
    Http::fake(['http://other.test/api/v1/invites/accept' => Http::response(['error' => 'Forbidden', 'code' => 'invite_refused', 'detail' => 'invite refused'], 403)]);
    fakeDirectorBackground();
    $before = app(ConnectionStore::class)->resolve();

    Livewire::test('pages::client.connect')
        ->set('url', 'http://other.test')
        ->set('inviteCode', 'csi_spent')
        ->set('inviteDevice', 'mac')
        ->call('acceptInvite')
        ->assertSee('That invite was not accepted');

    expect(app(ConnectionStore::class)->resolve())->toEqual($before);
});

it('says how long to wait when invite tries are throttled', function () {
    Http::fake(['http://other.test/api/v1/invites/accept' => Http::response(['error' => 'Too Many Requests', 'code' => 'rate_limited', 'detail' => 'slow down'], 429, ['Retry-After' => '42'])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->set('url', 'http://other.test')
        ->set('inviteCode', 'csi_x')
        ->set('inviteDevice', 'mac')
        ->call('acceptInvite')
        ->assertSee('Too many tries. Wait 42 seconds');
});

it('forgets who this device is when the stream says authorization_changed or revoked', function () {
    expect(EventStreamController::changesIdentity("id: 9\nevent: change\ndata: {\"kind\":\"authorization_changed\",\"id\":\"mem_me\",\"revision\":9}\n\n"))->toBeTrue()
        ->and(EventStreamController::changesIdentity("event: revoked\ndata: {}\n\n"))->toBeTrue()
        ->and(EventStreamController::changesIdentity("event: change\ndata: {\"kind\":\"job\"}\n\n"))->toBeFalse();
});

it('asks who this device is again after the relayed stream ends', function () {
    meOnDirector(meAs('owner'), meAs('viewer'));
    Http::fake([
        director('events') => Http::response("event: hello\ndata: {\"revision\":1}\n\nevent: change\ndata: {\"kind\":\"authorization_changed\",\"id\":\"mem_me\",\"revision\":2}\n\n"),
    ]);
    fakeDirectorBackground();
    expect(app(Identity::class)->role())->toBe('owner');

    $this->get(route('stream'))->streamedContent();

    expect(app(Identity::class)->role())->toBe('viewer');
});

it('asks an unreachable Director who this device is once per request, however many controls the page draws', function () {
    meOnDirector(['unreachable']);
    Http::fake([director('intents?*') => Http::response(['intents' => [], 'jobs' => []])]);
    fakeDirectorBackground();

    $this->get(route('intents.index'))->assertOk()->assertSee('New task');

    expect(Http::recorded(fn (Request $request) => str_ends_with($request->url(), '/me')))->toHaveCount(1);
});
