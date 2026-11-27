<?php

use App\Client\TailscaleAddress;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

/*
 * Fields the Director is adding (per-record actions, approval kind on attention, display names and the member
 * directory, role lists on me, paused_reason, a pairing link): each is read when present, and the page falls back
 * to its own rules when absent (the other tests cover that).
 */

beforeEach(fn () => connectClientWeb());

it('lets an approval\'s own decide action show or hide Approve and Deny', function (string $role, bool $decide) {
    meOnDirector(meAs($role));
    Http::fake([director('action-approvals') => Http::response(['action_approvals' => [[
        'id' => 'apr_1', 'job_id' => 'job_1', 'status' => 'needs_owner', 'argv' => ['git', 'push'], 'assignee_member_id' => 'mem_me',
        'created_at' => '2026-11-27T10:00:00Z', 'revision' => 3, 'actions' => ['decide' => $decide],
    ]]])]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.approvals');
    $decide ? $page->assertSeeHtml('data-test="approve-action"') : $page->assertDontSeeHtml('data-test="approve-action"');
})->with([['member', true], ['owner', false]]);

it('offers the roles me says this device can invite and assign', function () {
    meOnDirector([...meAs('admin'), 'invitable_roles' => ['viewer'], 'assignable_roles' => ['member', 'viewer', 'admin']]);
    Http::fake([
        director('members') => Http::response(['members' => []]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    $page = Livewire::test('pages::client.members');

    expect($page->instance()->roles(true))->toBe(['viewer'])
        ->and($page->instance()->roles())->toBe(['member', 'viewer', 'admin']);
});

it('shows a schedule\'s paused_reason without guessing from the member list', function () {
    meOnDirector(meAs('member'));
    Http::fake([director('schedules') => Http::response(['schedules' => [['schedule_key' => 'laptop/nightly', 'node' => 'laptop', 'schedule_id' => 'nightly',
        'enabled' => true, 'cron' => '0 2 * * *', 'responsible_member_id' => 'mem_sam', 'responsible_display_name' => 'Sam', 'paused_reason' => 'member_disabled', 'revision' => 7]]])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.schedules')
        ->assertSeeHtml('data-test="schedule-paused"')
        ->assertSee('is disabled')
        ->assertSee('Sam');
});

it('shows the recovery pairing link the Director writes from the address this desktop sends', function () {
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
        'http://127.0.0.1:8780/api/v1/client/pairings' => Http::response(['code' => 'PAIR-1', 'name' => 'ipad', 'expires_at' => '2026-11-27T10:10:00Z', 'member_id' => 'mem_sam',
            'link' => 'contextstack://pair?url=http%3A%2F%2F192.0.2.7%3A8780&code=PAIR-1']),
        'http://127.0.0.1:8780/api/v1/client/members' => Http::response(['members' => [['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'member', 'status' => 'active']]]),
        'http://127.0.0.1:8780/api/v1/client/invites' => Http::response(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->set('recoveryMember', 'mem_sam')
        ->set('recoveryName', 'ipad')
        ->call('recover')
        ->assertDispatched('cs-secret-issued', fn (string $name, array $params): bool => $params['link'] === 'contextstack://pair?url=http%3A%2F%2F192.0.2.7%3A8780&code=PAIR-1'
            && $params['address'] === 'http://192.0.2.7:8780');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/pairings') && $request->header('X-ContextStack-Invite-Origin') === ['http://192.0.2.7:8780']
        && $request->hasHeader('Idempotency-Key'));
});
