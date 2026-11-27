<?php

use App\Client\TailscaleAddress;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * @return list<array<string, string>>
 */
function teamMembers(): array
{
    return [
        ['id' => 'mem_me', 'display_name' => 'Robin', 'role' => 'owner', 'status' => 'active'],
        ['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'member', 'status' => 'active'],
        ['id' => 'mem_kim', 'display_name' => 'Kim', 'role' => 'viewer', 'status' => 'pending'],
    ];
}

it('lists members and open invites for an owner, without any code', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('members') => Http::response(['members' => teamMembers()]),
        director('invites') => Http::response(['invites' => [
            ['id' => 'inv_1', 'member_id' => 'mem_kim', 'role' => 'viewer', 'status' => 'pending', 'expires_at' => '2026-11-28T10:00:00Z', 'issued_by' => 'cred_this'],
        ]]),
    ]);

    Livewire::test('pages::client.members')
        ->assertSee('Sam')
        ->assertSee('invited, not joined yet')
        ->assertSeeHtml('data-test="invite-row"')
        ->assertSee('New invite code')
        ->assertDontSeeHtml('data-test="issued-code"');
});

it('tells a member or viewer that their role does not manage members, and asks the Director nothing more', function (string $role) {
    meOnDirector(meAs($role));

    Livewire::test('pages::client.members')->assertSeeHtml('data-test="members-not-allowed"');

    Http::assertNotSent(fn (Request $request) => $request->url() === director('members'));
})->with(['member', 'viewer']);

it('offers an admin only the member and viewer roles', function () {
    meOnDirector(meAs('admin'));
    Http::fake([
        director('members') => Http::response(['members' => [['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'member', 'status' => 'active']]]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    $page = Livewire::test('pages::client.members');

    expect($page->instance()->roles(true))->toBe(['member', 'viewer'])
        ->and($page->instance()->roles())->toBe(['member', 'viewer']);
});

it('hands a new invite code, link and QR to the browser once, keeping none of it in the component', function () {
    config(['client.director_url' => 'http://127.0.0.1:8780']);
    app()->instance(TailscaleAddress::class, new class extends TailscaleAddress
    {
        public function ipv4(): ?string
        {
            return '192.0.2.7';
        }
    });
    meOnDirector(meAs('owner'));
    Http::fake([
        'http://127.0.0.1:8780/api/v1/client/members/mem_sam' => Http::response(['member' => ['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'admin', 'status' => 'active']]),
        'http://127.0.0.1:8780/api/v1/client/members' => Http::response(['members' => teamMembers()]),
        // The Director names the host its own request came in on: loopback, useless to another device.
        'http://127.0.0.1:8780/api/v1/client/invites' => Http::sequence()
            ->push(['invites' => []])
            ->push(['id' => 'inv_2', 'member_id' => 'mem_new', 'role' => 'member', 'code' => 'csi_secret_code', 'expires_at' => '2026-11-28T10:00:00Z',
                'link' => 'contextstack://invite?url=http%3A%2F%2F127.0.0.1%3A8780&code=csi_secret_code'], 201)
            ->push(['invites' => []])
            ->push(['invites' => []])
            ->push(['invites' => []]),
    ]);

    $page = Livewire::test('pages::client.members')
        ->set('inviteName', 'Alex')
        ->set('inviteRole', 'member')
        ->call('invite')
        ->assertDispatched('cs-secret-issued', fn (string $name, array $params): bool => $params['code'] === 'csi_secret_code'
            && $params['link'] === 'contextstack://invite?url=http%3A%2F%2F192.0.2.7%3A8780&code=csi_secret_code'
            && str_contains($params['qr'], '<svg'));

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && str_ends_with($request->url(), '/invites')
        && $request['display_name'] === 'Alex' && $request->header('X-ContextStack-Invite-Origin') === ['http://192.0.2.7:8780']);

    // Never component state: not in the snapshot or the HTML of that answer, nor of the next action's.
    expect(json_encode($page->getData()))->not->toContain('csi_secret_code')
        ->and($page->html())->not->toContain('csi_secret_code');
    $page->call('changeRole', 'mem_sam', 'admin')->assertNotDispatched('cs-secret-issued');
    expect(json_encode($page->getData()))->not->toContain('csi_secret_code')
        ->and($page->html())->not->toContain('csi_secret_code');

    // A reload is a new page: the code is not kept anywhere to show again.
    Livewire::test('pages::client.members')->assertDontSee('csi_secret_code');
    expect(json_encode(session()->all()))->not->toContain('csi_secret_code');
});

it('reissues a pending member\'s code with only their member id', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('members') => Http::response(['members' => teamMembers()]),
        director('invites') => Http::sequence()
            ->push(['invites' => []])
            ->push(['id' => 'inv_3', 'member_id' => 'mem_kim', 'role' => 'viewer', 'code' => 'csi_fresh', 'expires_at' => '2026-11-28T10:00:00Z', 'link' => 'contextstack://invite?url=x&code=csi_fresh'], 201)
            ->push(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')->call('reissue', 'mem_kim')
        ->assertDispatched('cs-secret-issued', fn (string $name, array $params): bool => $params['code'] === 'csi_fresh'
            && $params['link'] === 'contextstack://invite?url=http%3A%2F%2Fdirector.test&code=csi_fresh');

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('invites') && $request->data() === ['member_id' => 'mem_kim']);
});

it('changes a role and disables a member with an Idempotency-Key', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('members/mem_sam') => Http::response(['member' => ['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'admin', 'status' => 'active']]),
        director('members') => Http::response(['members' => teamMembers()]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->call('changeRole', 'mem_sam', 'admin')
        ->call('setStatus', 'mem_sam', 'disabled')
        ->assertSet('problem', '');

    Http::assertSent(fn (Request $request) => $request->method() === 'PATCH' && $request->data() === ['role' => 'admin'] && $request->hasHeader('Idempotency-Key'));
    Http::assertSent(fn (Request $request) => $request->method() === 'PATCH' && $request->data() === ['status' => 'disabled'] && $request->hasHeader('Idempotency-Key'));
});

it('cancels an open invite', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('invites/inv_1') => Http::response(['id' => 'inv_1', 'revoked' => true]),
        director('members') => Http::response(['members' => teamMembers()]),
        director('invites') => Http::response(['invites' => [['id' => 'inv_1', 'member_id' => 'mem_kim', 'role' => 'viewer', 'status' => 'pending', 'expires_at' => '2026-11-28T10:00:00Z']]]),
    ]);

    Livewire::test('pages::client.members')->call('cancelInvite', 'inv_1');

    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && $request->url() === director('invites/inv_1'));
});

it('issues a recovery pairing code for a member and shows it once', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('pairings') => Http::response(['code' => 'PAIR-RECOVER', 'name' => 'ipad', 'expires_at' => '2026-11-27T10:10:00Z', 'member_id' => 'mem_sam']),
        director('members') => Http::response(['members' => teamMembers()]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->set('recoveryMember', 'mem_sam')
        ->set('recoveryName', 'ipad')
        ->call('recover')
        ->assertDispatched('cs-secret-issued', fn (string $name, array $params): bool => $params['code'] === 'PAIR-RECOVER'
            && $params['heading'] === 'Pairing code for Sam' && str_starts_with($params['link'], 'contextstack://pair?'))
        ->assertDontSee('PAIR-RECOVER');

    Http::assertSent(fn (Request $request) => $request->url() === director('pairings') && $request['member_id'] === 'mem_sam');
});

it('refreshes who this device is when a management action is refused after a downgrade', function () {
    meOnDirector(meAs('owner'), meAs('viewer'));
    Http::fake([
        director('members/mem_sam') => Http::response(['error' => 'Forbidden', 'code' => 'forbidden', 'detail' => 'member management is not permitted'], 403),
        director('members') => Http::response(['members' => teamMembers()]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->call('changeRole', 'mem_sam', 'admin')
        ->assertSee('member management is not permitted')
        ->assertSet('allowed', false)
        ->assertDontSee('Sam');
});

it('clears the page when the Director stops accepting this device (401)', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('members/mem_sam') => Http::response(['error' => 'Unauthorized', 'code' => 'unauthenticated', 'detail' => 'unknown credential'], 401),
        director('members') => Http::response(['members' => teamMembers()]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->assertSee('Sam')
        ->call('setStatus', 'mem_sam', 'disabled')
        ->assertSet('signedOut', true)
        ->assertSet('members', [])
        ->assertDispatched('cs-signed-out')
        ->assertDontSee('Sam');
});
