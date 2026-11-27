<?php

use Carbon\CarbonImmutable;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(function () {
    connectClientWeb();
    // The fixture's window ends 2026-11-28T10:00:00Z; the device clock sits before it unless a test moves it.
    $this->travelTo(CarbonImmutable::parse('2026-11-27T12:00:00Z'));
});

const BrowserSha = 'a3f1c2d4e5b6978877665544332211009988aabbccddeeff0011223344556677';

/**
 * A browser action approval as the Director projects it to an owner (CLIENT-API.md "Action approvals").
 *
 * @return array<string, mixed>
 */
function browserApproval(array $over = []): array
{
    return [...[
        'id' => 'apr_web', 'kind' => 'browser_action', 'job_id' => 'job_web', 'intent_id' => 'int_shop', 'run_id' => 'run_web', 'status' => 'needs_owner',
        'origin' => 'https://shop.example', 'action' => 'buy', 'target' => 'form#checkout <b>now</b>',
        'reason' => 'This browser action requires the owner\'s approval before it expires.',
        'field_values_digest' => str_repeat('d', 64), 'screenshot_sha' => BrowserSha,
        'values_summary' => [
            ['name' => 'item', 'hidden' => false, 'value' => 'Blue mug'],
            ['name' => 'item', 'hidden' => false, 'value' => 'Red mug'],
            ['name' => 'card', 'hidden' => false, 'length' => 16],
            ['name' => 'session', 'hidden' => true, 'value' => 'abc'],
        ],
        'expires_at' => '2026-11-28T10:00:00Z', 'created_at' => '2026-11-27T10:00:00Z', 'revision' => 2,
        'actions' => ['decide' => true, 'answer' => true, 'clear' => true],
    ], ...$over];
}

/**
 * @param  list<array<string, mixed>>  $approvals
 */
function fakeBrowserApprovals(array $approvals, array $more = []): void
{
    Http::fake([...$more, director('action-approvals') => Http::response(['action_approvals' => $approvals])]);
    fakeDirectorBackground();
}

it('shows the origin, action and exact target as plain text, every field in order, hidden labelled and masked as a count', function () {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval()]);

    Livewire::test('pages::client.approvals')
        ->assertSeeHtml('data-test="browser-approval"')
        ->assertDontSeeHtml('data-test="approval-command"')
        ->assertSee('https://shop.example')
        ->assertSee('buy')
        ->assertSee('form#checkout <b>now</b>')
        ->assertDontSeeHtml('<b>now</b>')
        ->assertSeeInOrder(['item', 'Blue mug', 'item', 'Red mug', 'card', 'masked, 16 characters', 'session', 'hidden', 'abc'])
        ->assertSee('4 fields it will send')
        ->assertSeeHtml('data-masked="true" data-hidden="false"')
        ->assertSeeHtml('datetime="2026-11-28T10:00:00Z"')
        ->assertSeeHtml('src="'.route('approvals.browser-screenshot', BrowserSha).'"')
        ->assertDontSee('csl_local_test');
});

it('labels an excluded per-load token when the Director marks it, and not otherwise', function () {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval(['values_summary' => [
        ['name' => 'csrf', 'hidden' => true, 'length' => 40, 'excluded' => true],
        ['name' => 'email', 'hidden' => false, 'value' => 'me@example.test'],
    ]])]);

    Livewire::test('pages::client.approvals')
        ->assertSeeInOrder(['csrf', 'hidden', 'excluded (per-load token)', 'masked, 40 characters', 'email', 'me@example.test'])
        ->assertSeeHtmlInOrder(['data-test="browser-field-excluded"', 'data-test="browser-field-name">email']);
});

it('shows the masked or hidden warning near the decide buttons when the Director sets it', function () {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval(['has_field_warning' => true, 'masked_or_hidden_count' => 2])]);

    Livewire::test('pages::client.approvals')
        ->assertSeeHtml('data-test="browser-field-warning"')
        ->assertSee('2 fields masked or hidden — review the screenshot')
        ->assertSeeHtmlInOrder(['data-test="browser-screenshot"', 'data-test="browser-field-warning"', 'data-test="approve-browser"']);
});

it('reads the outcome from the approval record and offers no approve again when the site may have acted', function (array $outcome, string $testId, string $message) {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval(['status' => 'approved', 'outcome' => $outcome, 'followup_job_id' => 'job_follow'])]);

    Livewire::test('pages::client.approvals')
        ->assertSeeHtml('data-test="'.$testId.'"')
        ->assertSee($message)
        ->assertDontSeeHtml('data-test="approve-browser"');
})->with([
    'submitted' => [['status' => 'submitted'], 'browser-outcome-submitted', 'The site accepted the submission.'],
    'legacy succeeded' => [['status' => 'succeeded'], 'browser-outcome-submitted', 'The site accepted the submission.'],
    'submission unknown' => [['status' => 'submission_unknown'], 'browser-outcome-unknown', 'Check the site before trying again.'],
    'refused with reason' => [['status' => 'refused', 'reason' => 'digest mismatch'], 'browser-outcome-refused', 'digest mismatch'],
]);

it('says a refused action is proposed again on the next run, and shows nothing for an unknown status', function () {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([
        browserApproval(['status' => 'approved', 'outcome' => ['status' => 'refused']]),
        browserApproval(['id' => 'apr_two', 'status' => 'approved', 'outcome' => ['status' => 'mystery']]),
    ]);

    Livewire::test('pages::client.approvals')
        ->assertSee('It will be proposed again on the next run.')
        ->assertDontSeeHtml('data-test="browser-outcome-unknown"')
        ->assertDontSeeHtml('data-test="browser-outcome-submitted"');
});

it('shows no warning when the Director sends none or says false', function (array $over) {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval($over)]);

    Livewire::test('pages::client.approvals')
        ->assertDontSeeHtml('data-test="browser-field-warning"')
        ->assertDontSee('review the screenshot');
})->with([
    'absent' => [[]],
    'false' => [['has_field_warning' => false, 'masked_or_hidden_count' => 3]],
]);

it('lets the record\'s actions.decide decide Approve and Deny, falling back to owner-only', function (string $role, ?bool $decide, bool $shown) {
    meOnDirector(meAs($role));
    $approval = browserApproval();
    if ($decide === null) {
        unset($approval['actions']);
    } else {
        $approval['actions'] = ['decide' => $decide, 'answer' => $decide];
    }
    fakeBrowserApprovals([$approval]);

    $page = Livewire::test('pages::client.approvals');
    if ($shown) {
        $page->assertSeeHtml('data-test="approve-browser"')->assertSeeHtml('data-test="deny-browser"');
    } else {
        $page->assertDontSeeHtml('data-test="approve-browser"')->assertSeeHtml('data-test="browser-read-only"');
    }
})->with([
    'owner told yes' => ['owner', true, true],
    'owner told no' => ['owner', false, false],
    'admin told yes' => ['admin', true, true],
    'admin without actions' => ['admin', null, false],
    'owner without actions' => ['owner', null, true],
]);

it('approves with a keyed decide POST and links the follow-up job\'s task, not a lane', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('action-approvals/apr_web/decide') => Http::response(browserApproval(['status' => 'approved', 'actions' => []])),
        director('action-approvals/apr_web') => Http::response(browserApproval(['status' => 'approved', 'decided_by' => 'cred_this',
            'decided_at' => '2026-11-27T10:02:00Z', 'followup_job_id' => 'job_follow', 'actions' => []])),
    ]);
    fakeBrowserApprovals([browserApproval()]);

    Livewire::test('pages::client.approvals')
        ->call('decideApproval', 'apr_web', 'approve')
        ->assertSeeHtml('data-status="approved"')
        ->assertSee('job_follow')
        ->assertSeeHtml('href="'.route('intents.show', 'int_shop').'"')
        ->assertDontSeeHtml('href="'.route('lanes.show', 'job_follow').'"')
        ->assertDontSeeHtml('data-test="approve-browser"')
        ->assertDontSeeHtml('data-test="browser-expires"');

    Http::assertSent(fn (Request $request): bool => $request->method() === 'POST' && $request->url() === director('action-approvals/apr_web/decide')
        && $request['decision'] === 'approve' && $request->hasHeader('Idempotency-Key'));
});

it('shows denied and expired proposals as closed, with nothing sent', function (string $status, string $words) {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval(['status' => $status, 'actions' => ['decide' => false, 'answer' => false]])]);

    Livewire::test('pages::client.approvals')
        ->assertSeeHtml('data-status="'.$status.'"')
        ->assertSee($words)
        ->assertDontSeeHtml('data-test="approve-browser"');
})->with([
    'denied' => ['denied', 'Denied. Nothing was sent.'],
    'expired' => ['expired', 'The window ended before it was decided, so nothing was sent.'],
]);

it('answers go or no_go from needs-you when the record says actions.answer', function () {
    meOnDirector(meAs('owner'));
    $conversation = ['conversation_id' => 'con_web', 'title' => 'Shopping', 'node' => 'workstation', 'model' => 'codex', 'state' => 'idle', 'lead' => true, 'spec' => 'spec.yaml',
        'attention' => ['question' => 'Buy on shop.example?', 'action_approval_id' => 'apr_web', 'kind' => 'browser_action',
            'options' => [['id' => 'go', 'label' => 'Go'], ['id' => 'no_go', 'label' => 'No go']], 'default' => 'no_go',
            'links' => ['approval' => 'apr_web'], 'actions' => ['answer' => true]],
        'queued_turn_ids' => [], 'turns' => 0, 'created_at' => '2026-11-27T09:00:00Z', 'revision' => 1];
    Http::fake([
        director('action-approvals/apr_web/answer') => Http::response(browserApproval(['status' => 'denied', 'actions' => []])),
        director('action-approvals/apr_web') => Http::response(browserApproval(['has_field_warning' => true, 'masked_or_hidden_count' => 1])),
        director('conversations/con_web/thread') => Http::response(['conversation' => $conversation, 'turns' => [], 'summaries' => []]),
        director('conversations/con_web/peer-messages') => Http::response(['peers' => []]),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.conversation', ['conversation' => 'con_web'])
        ->assertSeeHtml('data-test="browser-approval"')
        ->assertDontSeeHtml('data-test="needs-you-choices"')
        ->assertSee('1 field masked or hidden — review the screenshot')
        ->assertSeeHtml('data-test="browser-answer-go"')
        ->assertDontSeeHtml('data-test="approve-browser"')
        ->call('answerBrowserApproval', 'apr_web', 'no_go');

    Http::assertSent(fn (Request $request): bool => $request->method() === 'POST' && $request->url() === director('action-approvals/apr_web/answer')
        && $request['choice'] === 'no_go');
});

it('sends no answer for a choice outside go and no_go, or without actions.answer', function () {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval(), browserApproval(['id' => 'apr_noans', 'actions' => ['decide' => true]])]);

    Livewire::test('pages::client.approvals')
        ->call('answerBrowserApproval', 'apr_web', 'approve')
        ->call('answerBrowserApproval', 'apr_noans', 'go');

    Http::assertNotSent(fn (Request $request): bool => str_contains($request->url(), '/answer'));
});

it('relays the screenshot as an uncached JPEG, fetched with this device\'s credential', function () {
    $jpeg = "\xFF\xD8\xFF\xE0fixture";
    Http::fake([director('browser-action-screenshots/'.BrowserSha) => Http::response($jpeg, 200, ['Content-Type' => 'image/jpeg'])]);

    $response = $this->get(route('approvals.browser-screenshot', BrowserSha))
        ->assertOk()
        ->assertHeader('Content-Type', 'image/jpeg')
        ->assertHeader('X-Content-Type-Options', 'nosniff');

    expect($response->headers->get('Cache-Control'))->toContain('no-store')
        ->and($response->getContent())->toBe($jpeg)
        ->and($response->headers->all())->not->toHaveKey('authorization');
    expect(json_encode($response->headers->all()))->not->toContain('csl_local_test');
    Http::assertSent(fn (Request $request): bool => $request->url() === director('browser-action-screenshots/'.BrowserSha)
        && $request->hasHeader('Authorization', 'Bearer csl_local_test'));
});

it('404s a screenshot sha that is not lowercase sha256 without asking the Director', function (string $sha) {
    $this->get('/browser-action-screenshots/'.$sha)->assertNotFound();

    Http::assertNotSent(fn (Request $request): bool => str_contains($request->url(), 'browser-action-screenshots'));
})->with([
    'short' => [str_repeat('a', 63)],
    'long' => [str_repeat('a', 65)],
    'uppercase' => [strtoupper(BrowserSha)],
    'not hex' => [str_repeat('g', 64)],
    'traversal' => ['..%2F..%2Fme'],
]);

it('404s cleanly when the Director has no such screenshot or refuses it', function (int $status) {
    Http::fake([director('browser-action-screenshots/'.BrowserSha) => Http::response(['error' => 'x', 'code' => 'not_found'], $status)]);

    $this->get(route('approvals.browser-screenshot', BrowserSha))->assertNotFound();
})->with([404, 403]);

it('refuses to pass on anything the Director sends that is not a JPEG', function () {
    Http::fake([director('browser-action-screenshots/'.BrowserSha) => Http::response('<svg onload=alert(1)>', 200, ['Content-Type' => 'image/jpeg'])]);

    $this->get(route('approvals.browser-screenshot', BrowserSha))->assertStatus(502);
});

it('sends a device without a Director connection to connect, and asks the Director nothing', function () {
    config(['client.local_credential' => sys_get_temp_dir().'/client-web-missing-'.bin2hex(random_bytes(4))]);

    $this->get(route('approvals.browser-screenshot', BrowserSha))->assertRedirect(route('connect'));

    Http::assertNothingSent();
});

it('names the follow-up job without a link when the record carries no intent_id', function () {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval(['status' => 'approved', 'followup_job_id' => 'job_follow', 'intent_id' => null, 'actions' => []])]);

    Livewire::test('pages::client.approvals')
        ->assertSee('job_follow')
        ->assertDontSeeHtml('data-test="browser-followup-task"')
        ->assertDontSeeHtml('href="'.route('lanes.show', 'job_follow').'"');
});

it('hides Approve and Deny once expires_at passes by the device clock', function () {
    meOnDirector(meAs('owner'));
    $this->travelTo(CarbonImmutable::parse('2026-11-28T10:00:01Z'));
    fakeBrowserApprovals([browserApproval()]);

    Livewire::test('pages::client.approvals')
        ->assertSeeHtml('data-test="browser-window-ended"')
        ->assertDontSeeHtml('data-test="approve-browser"')
        ->assertDontSeeHtml('data-test="deny-browser"');
});

it('reads a decide the Director refuses with 409 after expiry again and shows it expired', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('action-approvals/apr_web/decide') => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'not decidable'], 409),
        director('action-approvals/apr_web') => Http::response(browserApproval(['status' => 'expired', 'actions' => ['decide' => false, 'answer' => false]])),
    ]);
    fakeBrowserApprovals([browserApproval()]);

    Livewire::test('pages::client.approvals')
        ->call('decideApproval', 'apr_web', 'approve')
        ->assertSeeHtml('data-status="expired"')
        ->assertSee('The window ended before it was decided, so nothing was sent.')
        ->assertSee('It expired, and nothing was sent.')
        ->assertDontSee('This request was already decided')
        ->assertDontSeeHtml('data-test="approve-browser"');

    Http::assertSent(fn (Request $request): bool => $request->method() === 'GET' && $request->url() === director('action-approvals/apr_web'));
});

it('reads an answer the Director refuses with 409 after expiry again and shows it expired', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('action-approvals/apr_web/answer') => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'not answerable'], 409),
        director('action-approvals/apr_web') => Http::response(browserApproval(['status' => 'expired', 'actions' => ['decide' => false, 'answer' => false]])),
    ]);
    fakeBrowserApprovals([browserApproval()]);

    Livewire::test('pages::client.approvals')
        ->call('answerBrowserApproval', 'apr_web', 'go')
        ->assertSeeHtml('data-status="expired"')
        ->assertSee('It expired, and nothing was sent.');

    Http::assertSent(fn (Request $request): bool => $request->method() === 'POST' && $request->url() === director('action-approvals/apr_web/answer') && $request['choice'] === 'go');
});

it('keeps the generic conflict words for a 409 that did not expire the proposal', function () {
    meOnDirector(meAs('owner'));
    Http::fake([
        director('action-approvals/apr_web/decide') => Http::response(['error' => 'Conflict', 'code' => 'conflict'], 409),
        director('action-approvals/apr_web') => Http::response(browserApproval(['status' => 'denied', 'decided_by' => 'cred_other', 'actions' => []])),
    ]);
    fakeBrowserApprovals([browserApproval()]);

    Livewire::test('pages::client.approvals')
        ->call('decideApproval', 'apr_web', 'approve')
        ->assertSeeHtml('data-status="denied"')
        ->assertSee('This request was already decided')
        ->assertDontSee('It expired, and nothing was sent.');
});

it('links the follow-up job\'s task from the record\'s intent_id without reading the job', function () {
    meOnDirector(meAs('owner'));
    fakeBrowserApprovals([browserApproval(['status' => 'approved', 'followup_job_id' => 'job_follow', 'actions' => []])]);

    Livewire::test('pages::client.approvals')
        ->assertSeeHtml('href="'.route('intents.show', 'int_shop').'"');

    Http::assertNotSent(fn (Request $request): bool => str_contains($request->url(), '/jobs/'));
});
