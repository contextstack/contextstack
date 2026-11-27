<?php

use App\NativeComponents\BrowserScreenshot;
use App\NativeComponents\Conversation;
use App\NativeComponents\Waiting;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Native\Mobile\Events\Biometric\Completed as BiometricCompleted;
use Native\Mobile\Testing\Native;

beforeEach(function () {
    isolateDevice();
});

/** A tiny stand-in JPEG: the magic bytes and some payload. */
function browserJpeg(): string
{
    return "\xFF\xD8\xFF\xE0".'stand-in screenshot bytes';
}

function browserSha(): string
{
    return hash('sha256', browserJpeg());
}

function pairBrowserOwner(bool $owner = true): void
{
    Http::fake([DirectorUrl.'/api/v1/client/me' => Http::response(['credential_id' => 'cred_ipad', 'owner' => $owner,
        'member' => ['id' => 'mem_me', 'display_name' => 'Sam'], 'role' => $owner ? 'owner' : 'admin',
        'capabilities' => ['read_work', 'start_work', 'control_work', 'approve_action', 'manage_members']])]);
    pairDevice();
    Http::fake([DirectorUrl.'/api/v1/client/members' => Http::response(['members' => []])]);
}

/**
 * @param  array<string, mixed>  $overrides
 * @return array<string, mixed>
 */
function browserApproval(array $overrides = []): array
{
    return array_merge(['id' => 'aa_b1', 'kind' => 'browser_action', 'status' => 'needs_owner', 'job_id' => 'job_9', 'run_id' => 'run_9',
        'reason' => 'This browser action requires the owner\'s approval before it expires.',
        'origin' => 'https://shop.example', 'action' => 'buy', 'target' => 'https://shop.example/checkout/confirm',
        'values_summary' => [
            ['name' => 'item', 'hidden' => false, 'value' => 'Blue mug'],
            ['name' => 'item', 'hidden' => false, 'value' => "Red\u{202E}mug"],
            ['name' => 'csrf', 'hidden' => true, 'length' => 40, 'excluded' => true],
            ['name' => 'card', 'hidden' => false, 'length' => 16],
        ],
        'screenshot_sha' => browserSha(), 'expires_at' => now()->addHours(5)->utc()->format('Y-m-d\TH:i:s\Z'),
        'created_at' => '2026-11-27T10:00:00Z', 'assignee_member_id' => 'mem_me', 'actions' => ['decide' => true, 'answer' => true]], $overrides);
}

/**
 * @param  list<array<string, mixed>>  $needsOwner
 */
function fakeBrowserInbox(array $needsOwner, ?string $jpeg = null): void
{
    Http::fake([
        DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response($jpeg ?? browserJpeg(), 200, ['Content-Type' => 'image/jpeg']),
        DirectorUrl.'/api/v1/client/action-approvals?*' => Http::response(['action_approvals' => $needsOwner]),
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
    ]);
}

function buttonLabelled(string $label): Closure
{
    return fn (array $node): bool => ($node['props']['label'] ?? null) === $label;
}

function screenshotImage(): Closure
{
    return fn (array $node): bool => ($node['props']['src'] ?? null) === 'data:image/jpeg;base64,'.base64_encode(browserJpeg());
}

it('shows the origin, action, exact target, every field in order and the screenshot', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval()]);

    Native::test(Waiting::class)
        ->assertSee('Buy on https://shop.example')
        ->assertSee('Browser action')
        ->call('select', 'aa_b1')
        ->assertSee('https://shop.example/checkout/confirm')
        ->assertSee('Fields, in page order (4)')
        ->assertSee('Blue mug')
        ->assertSee('Red\u{202E}mug')
        ->assertSee('csrf · hidden field · excluded (per-load token)')
        ->assertSee('masked, 40 characters')
        ->assertSee('masked, 16 characters')
        ->assertSee('Decide by')
        ->assertSee('Waiting on you. Nothing has been submitted.')
        ->assertSee('Tap to view full size')
        ->assertElement('image', screenshotImage())
        ->assertElement('button', buttonLabelled('Approve'))
        ->assertElement('button', buttonLabelled('Deny'))
        ->assertDontSee('masked or hidden — review the screenshot');

    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/browser-action-screenshots/'.browserSha())
        && $request->hasHeader('Accept', 'image/jpeg'));
});

it('keeps duplicate field names as separate entries in their order', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval()]);

    $tree = json_encode(Native::test(Waiting::class)->call('select', 'aa_b1')->tree());

    expect(substr_count($tree, '"item"'))->toBe(2)
        ->and(strpos($tree, 'Blue mug'))->toBeLessThan(strpos($tree, 'Red'))
        ->and(strpos($tree, 'Red'))->toBeLessThan(strpos($tree, '"csrf'))
        ->and(strpos($tree, '"csrf'))->toBeLessThan(strpos($tree, '"card"'));
});

it('reads the screenshot once while the screen polls', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval()]);

    Native::test(Waiting::class)->call('select', 'aa_b1')->call('refresh')->call('refresh');

    expect(collect(Http::recorded())->filter(fn (array $pair) => str_contains($pair[0]->url(), 'browser-action-screenshots'))->count())->toBe(1);
});

it('shows the masked-or-hidden warning from the Director when present', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval(['has_field_warning' => true, 'masked_or_hidden_count' => 2])]);

    Native::test(Waiting::class)->call('select', 'aa_b1')->assertSee('2 fields masked or hidden — review the screenshot');
});

it('takes the warning count from the Director, not from the entries shown', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval(['has_field_warning' => true, 'masked_or_hidden_count' => 7])]);

    Native::test(Waiting::class)->call('select', 'aa_b1')
        ->assertSee('7 fields masked or hidden — review the screenshot')
        ->assertDontSee('2 fields masked');
});

it('shows the expired state when a decision after the window gets a 409', function () {
    pairBrowserOwner();
    $state = ['record' => browserApproval()];
    Http::fake([
        DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response(browserJpeg(), 200, ['Content-Type' => 'image/jpeg']),
        DirectorUrl.'/api/v1/client/action-approvals/aa_b1/decide' => function () use (&$state) {
            $state['record'] = browserApproval(['status' => 'expired']);

            return Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'expired'], 409);
        },
        DirectorUrl.'/api/v1/client/action-approvals/aa_b1' => function () use (&$state) {
            return Http::response($state['record']);
        },
        DirectorUrl.'/api/v1/client/action-approvals?*' => function () use (&$state) {
            return Http::response(['action_approvals' => $state['record']['status'] === 'needs_owner' ? [$state['record']] : []]);
        },
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
    ]);

    Native::test(Waiting::class)->call('select', 'aa_b1')->tap('Approve')
        ->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-decide'])
        ->assertSet('selected', 'aa_b1')
        ->assertSee('Expired: the window ended before it was decided')
        ->assertSee('The decision window had ended, so the Director expired this request. Nothing was submitted.')
        ->assertMissingElement('button', buttonLabelled('Approve'))
        ->assertMissingElement('button', buttonLabelled('Deny'));
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/action-approvals/aa_b1') && $request->method() === 'GET');
});

it('refuses a screenshot that does not match its hash', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval()], "\xFF\xD8\xFF\xE0".'some other page');

    Native::test(Waiting::class)->call('select', 'aa_b1')
        ->assertSee('does not match the one this request was proposed with')
        ->assertMissingElement('image');
});

it('refuses a screenshot that is not a JPEG or is too large', function (string $type, int $size) {
    pairBrowserOwner();
    Http::fake([DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response(str_repeat('x', $size), 200, ['Content-Type' => $type])]);
    fakeBrowserInbox([browserApproval()]);

    Native::test(Waiting::class)->call('select', 'aa_b1')
        ->assertSee('The screenshot could not be read')
        ->assertMissingElement('image');
})->with([
    'html' => ['text/html', 10],
    'over 3 MiB' => ['image/jpeg', 3 * 1024 * 1024 + 1],
]);

it('does not fetch a screenshot whose SHA is malformed', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval(['screenshot_sha' => '../../etc/passwd'])]);

    Native::test(Waiting::class)->call('select', 'aa_b1')->assertSee('names no valid screenshot');
    Http::assertNotSent(fn (Request $request) => str_contains($request->url(), 'browser-action-screenshots'));
});

it('says when the screenshot is gone', function () {
    pairBrowserOwner();
    Http::fake([DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response(['error' => 'Not found', 'code' => 'not_found'], 404)]);
    fakeBrowserInbox([browserApproval()]);

    Native::test(Waiting::class)->call('select', 'aa_b1')->assertSee('The Director no longer has this screenshot.');
});

it('approves only after the device unlock and then shows the approved state', function () {
    pairBrowserOwner();
    $state = ['record' => browserApproval()];
    Http::fake([
        DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response(browserJpeg(), 200, ['Content-Type' => 'image/jpeg']),
        DirectorUrl.'/api/v1/client/action-approvals/aa_b1/decide' => function () use (&$state) {
            $state['record'] = browserApproval(['status' => 'approved', 'followup_job_id' => 'job_follow', 'decided_by' => 'cred_ipad']);

            return Http::response($state['record']);
        },
        DirectorUrl.'/api/v1/client/action-approvals/aa_b1' => function () use (&$state) {
            return Http::response($state['record']);
        },
        DirectorUrl.'/api/v1/client/action-approvals?*' => function () use (&$state) {
            return Http::response(['action_approvals' => $state['record']['status'] === 'needs_owner' ? [$state['record']] : []]);
        },
        DirectorUrl.'/api/v1/client/conversations?*' => Http::response(['conversations' => []]),
    ]);

    $screen = Native::test(Waiting::class)->call('select', 'aa_b1')->tap('Approve')->assertNativeCalled('Biometric.Prompt');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/decide'));

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-decide'])
        ->assertSet('selected', 'aa_b1')
        ->assertSee('Approved for one submission. Follow-up job job_follow carries it out in the same task')
        ->assertMissingElement('button', buttonLabelled('Approve'));
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/aa_b1/decide') && $request['decision'] === 'approve');

    // The follow-up has no lane of its own: Open the task goes to the proposing job's lane, which lists the task's jobs.
    $screen->tap('Open the task')->assertNavigatedTo('/lanes/job_9');
});

it('denies only after the device unlock, and sends nothing when the unlock fails', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval()]);
    Http::fake([DirectorUrl.'/api/v1/client/action-approvals/aa_b1/decide' => Http::response(browserApproval(['status' => 'denied']))]);

    Native::test(Waiting::class)->call('select', 'aa_b1')->tap('Deny')
        ->emitNative(BiometricCompleted::class, ['success' => false, 'id' => 'release-decide'])
        ->assertSee('Not unlocked, so nothing was sent.');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/decide'));
});

it('shows the denied and expired states', function (string $status, string $words) {
    pairBrowserOwner();
    fakeBrowserInbox([]);
    Http::fake([DirectorUrl.'/api/v1/client/action-approvals/aa_b1' => Http::response(browserApproval(['status' => $status]))]);

    Native::test(Waiting::class, data: ['approval' => 'aa_b1'])
        ->assertSee($words)
        ->assertMissingElement('button', buttonLabelled('Approve'));
})->with([
    'denied' => ['denied', 'Denied. Nothing was submitted.'],
    'expired' => ['expired', 'Expired: the window ended before it was decided'],
]);

it('offers Deny only once the window has ended', function () {
    pairBrowserOwner();
    fakeBrowserInbox([browserApproval(['expires_at' => '2026-01-01T00:00:00Z'])]);

    Native::test(Waiting::class)->call('select', 'aa_b1')
        ->assertSee('The window ended')
        ->assertSee('The decision window has ended, so it cannot be approved.')
        ->assertMissingElement('button', buttonLabelled('Approve'))
        ->assertSee('Deny');
});

it('offers no decision to a non-owner on a Director without actions', function () {
    pairBrowserOwner(owner: false);
    $approval = browserApproval();
    unset($approval['actions']);
    fakeBrowserInbox([$approval]);

    Native::test(Waiting::class)->call('select', 'aa_b1')
        ->assertSee('deciding it is not yours')
        ->assertMissingElement('button', buttonLabelled('Approve'));
});

it('shows the full-size screenshot on its own screen', function () {
    pairBrowserOwner();
    fakeBrowserInbox([]);

    Native::test(BrowserScreenshot::class, params: ['id' => browserSha()])
        ->assertElement('image', screenshotImage());
});

it('answers a browser needs-you item in a conversation only after the device unlock', function () {
    pairBrowserOwner();
    $attention = array_merge(browserApproval(), ['action_approval_id' => 'aa_b1', 'question' => 'This browser action requires the owner\'s approval before it expires.',
        'source_kind' => 'action_approval', 'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No-go']], 'default' => 'no_go',
        'links' => ['approval' => 'aa_b1', 'job' => 'job_9']]);
    unset($attention['status'], $attention['id']);
    Http::fake([
        DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response(browserJpeg(), 200, ['Content-Type' => 'image/jpeg']),
        DirectorUrl.'/api/v1/client/action-approvals/aa_b1/answer' => Http::response(['ok' => true]),
        DirectorUrl.'/api/v1/client/conversations/con_b/thread' => Http::response(['conversation' => ['conversation_id' => 'con_b', 'title' => 'Shopping', 'revision' => 1, 'attention' => $attention], 'turns' => []]),
        DirectorUrl.'/api/v1/client/conversations/con_b' => Http::response(['conversation' => ['conversation_id' => 'con_b', 'revision' => 1]]),
    ]);

    $screen = Native::test(Conversation::class, params: ['id' => 'con_b'])
        ->assertSee('Browser action approval')
        ->assertSee('https://shop.example/checkout/confirm')
        ->assertSee('masked, 16 characters')
        ->assertElement('image', screenshotImage())
        ->tap('Go')
        ->assertNativeCalled('Biometric.Prompt');
    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/answer'));

    $screen->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-answer']);
    Http::assertSent(fn (Request $request) => str_ends_with($request->url(), '/aa_b1/answer') && $request['choice'] === 'go');
});

it('does not offer Go on a needs-you item whose window has ended', function () {
    pairBrowserOwner();
    $attention = array_merge(browserApproval(['expires_at' => '2026-01-01T00:00:00Z']), ['action_approval_id' => 'aa_b1', 'question' => 'Approve?',
        'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No-go']], 'default' => 'no_go', 'links' => ['approval' => 'aa_b1']]);
    unset($attention['status'], $attention['id']);
    Http::fake([
        DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response(browserJpeg(), 200, ['Content-Type' => 'image/jpeg']),
        DirectorUrl.'/api/v1/client/conversations/con_b/thread' => Http::response(['conversation' => ['conversation_id' => 'con_b', 'revision' => 1, 'attention' => $attention], 'turns' => []]),
    ]);

    Native::test(Conversation::class, params: ['id' => 'con_b'])
        ->assertElement('button', buttonLabelled('No-go'))
        ->assertMissingElement('button', buttonLabelled('Go'));
});

it('shows the expired state when a needs-you answer after the window gets a 409', function () {
    pairBrowserOwner();
    $attention = array_merge(browserApproval(), ['action_approval_id' => 'aa_b1', 'question' => 'Approve?',
        'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No-go']], 'default' => 'no_go', 'links' => ['approval' => 'aa_b1']]);
    unset($attention['status'], $attention['id']);
    $state = ['attention' => $attention, 'revision' => 1];
    Http::fake([
        DirectorUrl.'/api/v1/client/browser-action-screenshots/*' => Http::response(browserJpeg(), 200, ['Content-Type' => 'image/jpeg']),
        DirectorUrl.'/api/v1/client/action-approvals/aa_b1/answer' => function () use (&$state) {
            $state = ['attention' => null, 'revision' => 2];

            return Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'expired'], 409);
        },
        DirectorUrl.'/api/v1/client/action-approvals/aa_b1' => Http::response(browserApproval(['status' => 'expired'])),
        DirectorUrl.'/api/v1/client/conversations/con_b/thread' => function () use (&$state) {
            return Http::response(['conversation' => ['conversation_id' => 'con_b', 'revision' => $state['revision'], 'attention' => $state['attention']], 'turns' => []]);
        },
        DirectorUrl.'/api/v1/client/conversations/con_b' => function () use (&$state) {
            return Http::response(['conversation' => ['conversation_id' => 'con_b', 'revision' => $state['revision']]]);
        },
    ]);

    Native::test(Conversation::class, params: ['id' => 'con_b'])
        ->tap('No-go')
        ->emitNative(BiometricCompleted::class, ['success' => true, 'id' => 'release-answer'])
        ->assertSee('Expired: the window ended before it was decided')
        ->assertSee('Expired. Nothing was submitted.')
        ->assertSee('The decision window had ended, so the Director expired this request.')
        ->assertMissingElement('button', buttonLabelled('No-go'));
});
