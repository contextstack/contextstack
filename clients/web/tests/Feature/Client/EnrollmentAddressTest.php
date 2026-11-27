<?php

use App\Client\TailscaleAddress;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

/*
 * Teams 2e: the owner's advertised URL (`GET`/`PUT advertised-url`) on Settings, invite and pairing links taken from
 * the Director's answer (X-ContextStack-Invite-Origin only when nothing is advertised), and member rename
 * (`PATCH members/{id} {display_name}`).
 */
beforeEach(fn () => connectClientWeb());

function onLoopbackWithTailscale(): void
{
    config(['client.director_url' => 'http://127.0.0.1:8780']);
    app()->instance(TailscaleAddress::class, new class extends TailscaleAddress
    {
        public function ipv4(): ?string
        {
            return '192.0.2.7';
        }
    });
}

/**
 * @return list<array<string, string>>
 */
function renameMembers(): array
{
    return [
        ['id' => 'mem_me', 'display_name' => 'Robin', 'role' => 'owner', 'status' => 'active'],
        ['id' => 'mem_sam', 'display_name' => 'Sam', 'role' => 'member', 'status' => 'active'],
    ];
}

it('shows the owner the advertised URL and saves a new one', function () {
    meOnDirector(meAs('owner'));
    advertisedUrlOnDirector('https://old.example.ts.net');
    Http::fake([director('advertised-url') => Http::response(['url' => 'https://director.example.ts.net'])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->assertSeeHtml('data-test="advertised-url"')
        ->assertSee('https://old.example.ts.net')
        ->assertDontSeeHtml('data-test="advertised-url-suggestion"')
        ->set('advertisedInput', ' https://director.example.ts.net ')
        ->call('saveAdvertised')
        ->assertSet('advertised', 'https://director.example.ts.net')
        ->assertSet('advertisedProblem', '');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->url() === director('advertised-url')
        && $request->data() === ['url' => 'https://director.example.ts.net']);
});

it('shows the Director\'s 422 for an address that is not an origin', function () {
    meOnDirector(meAs('owner'));
    Http::fake([director('advertised-url') => Http::response(['error' => 'Unprocessable Entity', 'code' => 'invalid_request', 'detail' => 'url must be an HTTP or HTTPS origin'], 422)]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->set('advertisedInput', 'https://director.example.ts.net/path')
        ->call('saveAdvertised')
        ->assertSet('advertised', '')
        ->assertSee('url must be an HTTP or HTTPS origin');
});

it('clears the advertised URL with null', function () {
    meOnDirector(meAs('owner'));
    advertisedUrlOnDirector('https://director.example.ts.net');
    Http::fake([director('advertised-url') => Http::response(['url' => null])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->call('clearAdvertised')
        ->assertSet('advertised', '');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->body() === '{"url":null}');
});

it('offers this machine\'s Tailscale address when nothing is advertised, and saves it in one click', function () {
    onLoopbackWithTailscale();
    meOnDirector(meAs('owner'));
    Http::fake(['http://127.0.0.1:8780/api/v1/client/advertised-url' => Http::response(['url' => 'http://192.0.2.7:8780'])]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->assertSeeHtml('data-test="advertised-url-suggestion"')
        ->assertSee('http://192.0.2.7:8780')
        ->call('useSuggestedAddress')
        ->assertSet('advertised', 'http://192.0.2.7:8780');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->data() === ['url' => 'http://192.0.2.7:8780']);
});

it('hides the advertised URL from everyone but the owner, and never asks for it', function (string $role) {
    meOnDirector(meAs($role));
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')->assertDontSeeHtml('data-test="advertised-url"');

    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/advertised-url'));
})->with(['admin', 'member']);

it('sends no origin header for an invite when an advertised URL is set, and shows the Director\'s link', function () {
    onLoopbackWithTailscale();
    meOnDirector(meAs('owner'));
    advertisedUrlOnDirector('https://director.example.ts.net');
    $link = 'contextstack://invite?url=https%3A%2F%2Fdirector.example.ts.net&code=csi_code';
    Http::fake([
        'http://127.0.0.1:8780/api/v1/client/members' => Http::response(['members' => renameMembers()]),
        'http://127.0.0.1:8780/api/v1/client/invites' => Http::sequence()
            ->push(['invites' => []])
            ->push(['id' => 'inv_1', 'member_id' => 'mem_new', 'role' => 'member', 'code' => 'csi_code', 'expires_at' => '2026-11-28T10:00:00Z', 'link' => $link], 201)
            ->push(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->set('inviteName', 'Alex')
        ->call('invite')
        ->assertDispatched('cs-secret-issued', fn (string $name, array $params): bool => $params['link'] === $link && $params['address'] === 'https://director.example.ts.net');

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && str_ends_with($request->url(), '/invites') && ! $request->hasHeader('X-ContextStack-Invite-Origin'));
});

it('invites with the advertised URL even when this machine has no Tailscale address', function () {
    config(['client.director_url' => 'http://127.0.0.1:8780']);
    app()->instance(TailscaleAddress::class, new class extends TailscaleAddress
    {
        public function ipv4(): ?string
        {
            return null;
        }
    });
    meOnDirector(meAs('owner'));
    advertisedUrlOnDirector('https://director.example.ts.net');
    Http::fake([
        'http://127.0.0.1:8780/api/v1/client/members' => Http::response(['members' => renameMembers()]),
        'http://127.0.0.1:8780/api/v1/client/invites' => Http::sequence()
            ->push(['invites' => []])
            ->push(['id' => 'inv_1', 'member_id' => 'mem_new', 'role' => 'member', 'code' => 'csi_code', 'expires_at' => '2026-11-28T10:00:00Z',
                'link' => 'contextstack://invite?url=https%3A%2F%2Fdirector.example.ts.net&code=csi_code'], 201)
            ->push(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->set('inviteName', 'Alex')
        ->call('invite')
        ->assertSet('problem', '')
        ->assertDispatched('cs-secret-issued');
});

it('pairs a device with the Director\'s link and sends this desktop\'s address only when nothing is advertised', function (?string $advertised, string $linkUrl, bool $header) {
    onLoopbackWithTailscale();
    advertisedUrlOnDirector($advertised);
    meOnDirector(meAs('owner'));
    Http::fake([
        'http://127.0.0.1:8780/api/v1/client/credentials' => Http::response(['credentials' => []]),
        'http://127.0.0.1:8780/api/v1/client/pairings' => Http::response(['code' => 'PAIR-2', 'name' => 'ipad', 'expires_at' => '2026-11-27T10:10:00Z',
            'link' => 'contextstack://pair?url='.rawurlencode($linkUrl).'&code=PAIR-2']),
    ]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.devices')->set('name', 'ipad')->call('pair');

    expect($page->get('issued')['link'])->toBe('contextstack://pair?url='.rawurlencode($linkUrl).'&code=PAIR-2');
    $page->assertSee($linkUrl)->assertSeeHtml('data-test="pairing-qr"');
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/pairings')
        && $request->hasHeader('X-ContextStack-Invite-Origin') === $header
        && (! $header || $request->header('X-ContextStack-Invite-Origin') === ['http://192.0.2.7:8780']));
})->with([
    'advertised' => ['https://director.example.ts.net', 'https://director.example.ts.net', false],
    'not advertised' => [null, 'http://192.0.2.7:8780', true],
]);

it('builds the pairing link itself when the Director sends none, and never shows a loopback one', function (?string $link) {
    onLoopbackWithTailscale();
    Http::fake([
        'http://127.0.0.1:8780/api/v1/client/credentials' => Http::response(['credentials' => []]),
        'http://127.0.0.1:8780/api/v1/client/pairings' => Http::response(array_filter(['code' => 'PAIR-3', 'name' => 'ipad', 'expires_at' => '2026-11-27T10:10:00Z', 'link' => $link])),
    ]);

    $page = Livewire::test('pages::client.devices')->set('name', 'ipad')->call('pair');

    expect($page->get('issued')['link'])->toBe('contextstack://pair?url=http%3A%2F%2F192.0.2.7%3A8780&code=PAIR-3');
})->with([
    'no link' => [null],
    'loopback link' => ['contextstack://pair?url=http%3A%2F%2F127.0.0.1%3A8780&code=PAIR-3'],
]);

it('renames another member with display_name alone and an Idempotency-Key', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('members/mem_sam') => Http::response(['member' => ['id' => 'mem_sam', 'display_name' => 'Samira', 'role' => 'member', 'status' => 'active']]),
        director('members') => Http::response(['members' => renameMembers()]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    Livewire::test('pages::client.members')
        ->assertSeeHtml('data-test="rename"')
        ->call('startRename', 'mem_sam')
        ->assertSet('renameName', 'Sam')
        ->set('renameName', '  Samira ')
        ->call('rename')
        ->assertHasNoErrors()
        ->assertSet('problem', '');

    Http::assertSent(fn (Request $request) => $request->method() === 'PATCH' && $request->url() === director('members/mem_sam')
        && $request->data() === ['display_name' => 'Samira'] && $request->hasHeader('Idempotency-Key'));
});

it('lets a member without member management rename only themself', function () {
    meOnDirector(meAs('member', 'mem_me', 'Robin'));
    Http::fake([
        director('members/mem_me') => Http::response(['member' => ['id' => 'mem_me', 'display_name' => 'Rob', 'role' => 'member', 'status' => 'active']]),
    ]);

    $page = Livewire::test('pages::client.members')
        ->assertSeeHtml('data-test="members-not-allowed"')
        ->assertSeeHtml('data-test="own-name"')
        ->call('startRename', 'mem_sam')
        ->assertSet('renameMember', '')
        ->call('startRename', 'mem_me')
        ->assertSet('renameName', 'Robin')
        ->set('renameName', 'Rob')
        ->call('rename')
        ->assertSet('problem', '');

    Http::assertSent(fn (Request $request) => $request->method() === 'PATCH' && $request->url() === director('members/mem_me') && $request->data() === ['display_name' => 'Rob']);
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/members/mem_sam'));
});

it('refuses an empty or over-long name before asking, and shows the Director\'s refusal', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('members/mem_sam') => Http::response(['error' => 'Forbidden', 'code' => 'forbidden', 'detail' => 'cannot rename that member'], 403),
        director('members') => Http::response(['members' => renameMembers()]),
        director('invites') => Http::response(['invites' => []]),
    ]);

    $page = Livewire::test('pages::client.members')->call('startRename', 'mem_sam');
    $page->set('renameName', '   ')->call('rename')->assertHasErrors('renameName');
    $page->set('renameName', str_repeat('é', 51))->call('rename')->assertHasErrors('renameName');
    Http::assertNotSent(fn (Request $request) => $request->method() === 'PATCH');

    $page->set('renameName', 'Samira')->call('rename')->assertSee('cannot rename that member');
});
