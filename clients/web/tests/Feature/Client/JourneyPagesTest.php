<?php

use App\Client\Bundle;
use App\Client\DirectorClient;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * @return array<string, mixed>
 */
function intentRecord(array $over = []): array
{
    return [...['intent_id' => 'int_1', 'title' => 'say hello', 'node' => 'laptop', 'spec' => 'spec.yaml', 'task' => 'tasks/hello',
        'trials' => 1, 'grant' => null, 'meta' => [], 'created_at' => '2026-11-10T10:00:00Z', 'job_ids' => ['job_1'], 'project_id' => null,
        'revision' => 5], ...$over];
}

/**
 * @return array<string, mixed>
 */
function jobRecord(array $over = []): array
{
    return [...['job_id' => 'job_1', 'intent_id' => 'int_1', 'node' => 'laptop', 'state' => 'accepted', 'created_at' => '2026-11-10T10:00:00Z',
        'accepted_at' => '2026-11-10T10:00:05Z', 'finished_at' => null, 'run_id' => null, 'run_ids' => [], 'cancel' => null,
        'live_run_ids' => [], 'budget' => null, 'revision' => 6], ...$over];
}

/**
 * @return array<string, mixed>
 */
function runRecord(array $over = []): array
{
    return [...['run_id' => 'run_1', 'job_id' => 'job_1', 'intent_id' => 'int_1', 'node' => 'laptop', 'verdict' => 'pass',
        'usage' => ['input_tokens' => 1200, 'output_tokens' => 300, 'est_usd' => 0.0045, 'charged_usd' => 0.0123,
            'source' => ['class' => 'api', 'model' => 'gpt-test']],
        'files' => [['path' => 'transcript.md', 'sha256' => str_repeat('a', 64), 'bytes' => 13], ['path' => 'check.json', 'sha256' => str_repeat('b', 64), 'bytes' => 14]],
        'received_at' => '2026-11-10T10:01:00Z', 'decision' => null, 'events' => 0, 'revision' => 9], ...$over];
}

function fakeIntent(array $jobs, array $runs = [], array $more = []): void
{
    Http::fake([
        director('intents/int_1') => Http::response(['intent' => intentRecord(), 'jobs' => $jobs, 'runs' => $runs]),
        director('usage*') => Http::response(['total' => ['runs' => count($runs), 'charged_usd' => 0.0123, 'kinds' => []]]),
        ...$more,
    ]);
}

it('submits an intent: uploads the bundle files, lists them, and opens the recorded intent', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]], 'stale_after_seconds' => 300]),
        director('objects/*') => Http::response(['stored' => true]),
        director('intents') => Http::response(['intent' => intentRecord(), 'jobs' => [jobRecord(['state' => 'offered'])], 'runs' => []], 201),
    ]);

    $page = Livewire::test('pages::client.intent-create')
        ->assertSet('node', 'laptop')
        ->set('title', 'say hello')
        ->set('goal', "Say hello.\n")
        ->set('check', "#!/bin/sh\ntrue\n");
    $key = $page->get('submitKey');

    $page->call('submit')->assertHasNoErrors()->assertRedirect(route('intents.show', 'int_1'));

    Http::assertSent(function (Request $request) use ($key) {
        if ($request->method() !== 'POST' || $request->url() !== director('intents')) {
            return false;
        }
        $files = collect($request['files'])->keyBy('path');

        return $request->hasHeader('Idempotency-Key', $key) && $request['node'] === 'laptop' && $request['spec'] === 'spec.yaml'
            && $request['task'] === 'tasks/hello' && $request['title'] === 'say hello'
            && $files->keys()->sort()->values()->all() === ['prompts/system.md', 'spec.yaml', 'tasks/hello/check', 'tasks/hello/goal.md']
            && $files['tasks/hello/check']['mode'] === 493 && $files['tasks/hello/goal.md']['sha256'] === hash('sha256', "Say hello.\n");
    });
    expect(Http::recorded()->filter(fn (array $pair) => $pair[0]->method() === 'PUT')->count())->toBe(4);
});

it('leaves the check to the Director when the form has none', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('objects/*') => Http::response(['stored' => true]),
        director('intents') => Http::response(['intent' => intentRecord(), 'jobs' => [jobRecord(['state' => 'offered'])], 'runs' => []], 201),
    ]);

    Livewire::test('pages::client.intent-create')->set('goal', "Say hello.\n")
        ->call('submit')->assertHasNoErrors()->assertRedirect(route('intents.show', 'int_1'));

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('intents')
        && ! in_array('tasks/hello/check', collect($request['files'])->pluck('path')->all(), true));
});

it('keeps the key when the answer is lost, so submitting again replays the same request', function () {
    $posts = 0;
    Http::fake(function (Request $request) use (&$posts) {
        if ($request->method() === 'GET') {
            return Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]], 'projects' => []]);
        }
        if ($request->method() === 'PUT') {
            return Http::response(['stored' => true]);
        }
        if (++$posts === 1) {
            throw new ConnectionException('timed out');
        }

        return Http::response(['intent' => intentRecord(), 'jobs' => [], 'runs' => []], 200, ['Idempotent-Replayed' => 'true']);
    });

    $page = Livewire::test('pages::client.intent-create')->set('goal', 'Say hello.');
    $key = $page->get('submitKey');
    $page->call('submit')->assertSee('Submitting again is safe')->assertSet('submitKey', $key);
    $page->call('submit')->assertRedirect(route('intents.show', 'int_1'));

    $keys = Http::recorded()->filter(fn (array $pair) => $pair[0]->method() === 'POST')->map(fn (array $pair) => $pair[0]->header('Idempotency-Key')[0]);
    expect($keys->unique()->values()->all())->toBe([$key]);
});

it('shows the recorded intent and takes a new key when the key named a different request', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('objects/*') => Http::response(['stored' => true]),
        director('intents') => Http::response(['error' => 'Conflict', 'code' => 'idempotency_conflict', 'detail' => 'different request',
            'record' => ['kind' => 'intent', 'id' => 'int_9']], 409),
    ]);

    $page = Livewire::test('pages::client.intent-create')->set('goal', 'Say hello.');
    $key = $page->get('submitKey');
    $page->call('submit')->assertSee('Open the recorded task')->assertSee(route('intents.show', 'int_9'), false);

    expect($page->get('submitKey'))->not->toBe($key);
});

it('shows the Director\'s reason for an unusable intent', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('objects/*') => Http::response(['stored' => true]),
        director('intents') => Http::response(['error' => 'Unprocessable', 'code' => 'invalid_request', 'detail' => 'tasks/hello/goal.md is not in the bundle'], 422),
    ]);

    Livewire::test('pages::client.intent-create')->set('goal', 'x')->call('submit')->assertSee('tasks/hello/goal.md is not in the bundle');
});

it('shows the intent, its job, its runs with usage, and never an estimated price', function () {
    fakeIntent([jobRecord(['state' => 'done', 'run_id' => 'run_1', 'run_ids' => ['run_1']])], [runRecord()]);

    $this->get(route('intents.show', 'int_1'))->assertOk()
        ->assertSee('say hello')->assertSee('run_1')->assertSee('$0.0123 charged')->assertSee('gpt-test')
        ->assertDontSeeText('0.0045')->assertSee('Run again');
});

it('answers 404 for an intent the Director does not know', function () {
    Http::fake([director('intents/nope') => Http::response(['error' => 'Not Found', 'code' => 'not_found', 'detail' => 'no such intent'], 404)]);

    $this->get(route('intents.show', 'nope'))->assertNotFound();
});

it('applies a change only when it is newer than the record held, and ignores other intents', function () {
    fakeIntent([jobRecord()]);
    $page = Livewire::test('pages::client.intent', ['intent' => 'int_1']);

    $page->call('applyStream', [
        ['kind' => 'job', 'id' => 'job_1', 'revision' => 4, 'record' => jobRecord(['state' => 'offered', 'revision' => 4])],
        ['kind' => 'job', 'id' => 'job_9', 'revision' => 20, 'record' => jobRecord(['job_id' => 'job_9', 'intent_id' => 'int_other', 'revision' => 20])],
    ]);
    expect($page->get('records')['job:job_1']['state'])->toBe('accepted')
        ->and($page->get('records'))->not->toHaveKey('job:job_9');

    $page->call('applyStream', [['kind' => 'job', 'id' => 'job_1', 'revision' => 12, 'record' => jobRecord(['state' => 'done', 'revision' => 12])]]);
    expect($page->get('records')['job:job_1']['state'])->toBe('done');
});

it('catches up a run\'s live output, then appends stream events above the last seq, until the result replaces it', function () {
    $live = jobRecord(['live_run_ids' => ['run_1'], 'revision' => 7]);
    fakeIntent([$live], [], [
        director('runs/run_1/events*') => Http::response(['run_id' => 'run_1', 'finished' => false, 'confirmed' => 2,
            'events' => [['seq' => 1, 'kind' => 'delta', 'partial' => true, 'payload' => ['text' => 'Hel']], ['seq' => 2, 'kind' => 'delta', 'partial' => true, 'payload' => ['text' => 'lo']]]]),
    ]);

    $page = Livewire::test('pages::client.intent', ['intent' => 'int_1'])->assertSee('Hello');

    $page->call('applyStream', [], [
        ['run_id' => 'run_1', 'job_id' => 'job_1', 'seq' => 2, 'kind' => 'delta', 'payload' => ['text' => 'lo']],
        ['run_id' => 'run_1', 'job_id' => 'job_1', 'seq' => 3, 'kind' => 'delta', 'payload' => ['text' => ', world']],
        ['run_id' => 'run_x', 'job_id' => 'job_other', 'seq' => 1, 'kind' => 'delta', 'payload' => ['text' => 'not ours']],
    ])->assertSee('Hello, world')->assertDontSee('not ours');

    $page->call('applyStream', [['kind' => 'run', 'id' => 'run_1', 'revision' => 30, 'record' => runRecord(['revision' => 30])]])
        ->assertSee('The run is reported; its evidence is the result.')->assertDontSee('Hello, world');
});

it('requests cancel with a key and shows it as stopping until the node reports an outcome', function () {
    $requested = ['cancel_id' => 'can_1', 'job_id' => 'job_1', 'state' => 'requested', 'outcome' => null,
        'pending' => true, 'detail' => 'waiting for the node to pick it up', 'revision' => 20];
    Http::fake([
        director('intents/int_1') => Http::sequence()
            ->push(['intent' => intentRecord(), 'jobs' => [jobRecord()], 'runs' => []])
            ->push(['intent' => intentRecord(), 'jobs' => [jobRecord(['cancel' => ['cancel_id' => 'can_1', 'state' => 'requested', 'outcome' => null], 'revision' => 20])], 'runs' => []]),
        director('usage*') => Http::response(['total' => ['runs' => 0, 'kinds' => []]]),
        director('jobs/job_1/cancel') => Http::response($requested, 202),
        director('cancels/can_1') => Http::response($requested),
    ]);

    $page = Livewire::test('pages::client.intent', ['intent' => 'int_1']);
    $key = $page->get('cancelKey');

    $page->set('cancelReasons.job_1', 'wrong task')->call('requestCancel', 'job_1')
        ->assertSee('stopping')->assertSee('waiting for the node to pick it up');
    Http::assertSent(fn (Request $request) => $request->url() === director('jobs/job_1/cancel')
        && $request->hasHeader('Idempotency-Key', $key) && $request['reason'] === 'wrong task');

    $page->call('applyStream', [['kind' => 'cancel', 'id' => 'can_1', 'revision' => 25, 'record' => ['cancel_id' => 'can_1', 'job_id' => 'job_1',
        'state' => 'applied', 'outcome' => 'stopped', 'detail' => 'stopped by the runner', 'revision' => 25]]])
        ->assertSee('stopped by the runner')->assertDontSee('stopping (');
});

it('says so when a cancel reaches a job that is already over', function () {
    fakeIntent([jobRecord()], [], [
        director('jobs/job_1/cancel') => Http::response(['error' => 'Conflict', 'code' => 'job_finished', 'detail' => 'the job is done'], 409),
    ]);

    Livewire::test('pages::client.intent', ['intent' => 'int_1'])->call('requestCancel', 'job_1')
        ->assertSee('The job is already over; there is nothing to cancel.');
});

it('runs a finished job again by naming it, under a new key', function () {
    fakeIntent([jobRecord(['state' => 'done'])], [], [
        director('intents') => Http::response(['intent' => intentRecord(['intent_id' => 'int_2']), 'jobs' => [], 'runs' => []], 201),
    ]);

    Livewire::test('pages::client.intent', ['intent' => 'int_1'])->call('runAgain', 'job_1')->assertRedirect(route('intents.show', 'int_2'));

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['from_job'] === 'job_1'
        && $request['node'] === 'laptop' && ! isset($request['files']) && $request->hasHeader('Idempotency-Key'));
});

it('runs again by naming the job when its bundle has a check', function () {
    $files = [['path' => 'spec.yaml', 'sha256' => str_repeat('a', 64), 'mode' => 420], ['path' => 'tasks/hello/check', 'sha256' => str_repeat('c', 64), 'mode' => 493],
        ['path' => 'tasks/hello/goal.md', 'sha256' => str_repeat('b', 64), 'mode' => 420]];
    fakeIntent([jobRecord(['state' => 'done', 'files' => $files])], [], [
        director('intents') => Http::response(['intent' => intentRecord(['intent_id' => 'int_2']), 'jobs' => [], 'runs' => []], 201),
    ]);

    Livewire::test('pages::client.intent', ['intent' => 'int_1'])->call('runAgain', 'job_1')->assertRedirect(route('intents.show', 'int_2'));

    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['from_job'] === 'job_1' && ! isset($request['files']));
    expect(session('notice'))->toBeNull();
});

it('says so when the Director added the "no check" check to an old bundle it runs again', function () {
    fakeIntent([jobRecord(['state' => 'error'])], [], [
        director('intents') => Http::response(['intent' => intentRecord(['intent_id' => 'int_2']),
            'jobs' => [jobRecord(['job_id' => 'job_2', 'intent_id' => 'int_2', 'state' => 'offered', 'meta' => ['check_added' => 'none']])], 'runs' => []], 201),
    ]);

    Livewire::test('pages::client.intent', ['intent' => 'int_1'])->call('runAgain', 'job_1')->assertRedirect(route('intents.show', 'int_2'));

    // Only the job is named: the Director patches the bundle, not the app.
    Http::assertSent(fn (Request $request) => $request->url() === director('intents') && $request['from_job'] === 'job_1' && ! isset($request['files']));
    Http::assertNotSent(fn (Request $request) => str_contains($request->url(), 'objects/'));
    expect(session('notice'))->toContain('no check');
});

it('shows a run\'s evidence and usage, and approves its result with a key, showing the receipt', function () {
    Http::fake([
        director('runs/run_1') => Http::sequence()->push(runRecord())
            ->push(runRecord(['decision' => ['decision_id' => 'dec_1', 'decision' => 'approve', 'decided_at' => '2026-11-10T10:02:00Z'], 'revision' => 40])),
        director('runs/run_1/files/transcript.md') => Http::response("# transcript\n<script>x</script>", 200, ['Content-Type' => 'text/plain; charset=utf-8']),
        director('runs/run_1/decision') => Http::response(['decision_id' => 'dec_1', 'run_id' => 'run_1', 'decision' => 'approve', 'note' => 'looks right',
            'decided_by' => 'cred_local', 'decided_by_name' => 'local', 'decided_at' => '2026-11-10T10:02:00Z', 'revision' => 40,
            'scope' => 'run_result', 'authorises' => []], 201),
    ]);

    $page = Livewire::test('pages::client.run', ['run' => 'run_1'])
        ->assertSee('transcript.md')->assertSee('1,200')->assertSee('$0.0123')->assertDontSeeText('0.0045');
    $page->call('showFile', 'transcript.md')->assertSee('&lt;script&gt;x&lt;/script&gt;', false);

    $key = $page->get('decisionKey');
    $page->set('note', 'looks right')->call('approve')
        ->assertSee('Approved')->assertSee('dec_1')->assertSee('It authorises nothing further');
    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('runs/run_1/decision')
        && $request->hasHeader('Idempotency-Key', $key) && $request['decision'] === 'approve' && $request['note'] === 'looks right');
});

it('shows the decision on record when the run was already decided from another device', function () {
    Http::fake([
        director('runs/run_1') => Http::response(runRecord()),
        director('runs/run_1/decision') => Http::response(['error' => 'Conflict', 'code' => 'already_decided', 'detail' => 'decided',
            'decision' => ['decision_id' => 'dec_7', 'decision' => 'reject', 'decided_by_name' => 'phone', 'decided_at' => '2026-11-10T10:03:00Z', 'authorises' => []]], 409),
    ]);

    Livewire::test('pages::client.run', ['run' => 'run_1'])->call('approve')
        ->assertSee('already decided')->assertSee('Rejected')->assertSee('dec_7')->assertSee('phone');
});

it('refuses to show a file the run does not list', function () {
    Http::fake([director('runs/run_1') => Http::response(runRecord())]);

    Livewire::test('pages::client.run', ['run' => 'run_1'])->call('showFile', '../secret')->assertStatus(404);
});

it('lists the fleet, showing a node never heard from as stale, never stopped', function () {
    Http::fake([director('nodes') => Http::response(['stale_after_seconds' => 300, 'nodes' => [
        ['name' => 'laptop', 'stale' => false, 'listening' => true, 'protocol' => 2, 'open_jobs' => 1, 'last_seen_at' => '2026-11-10T10:00:00Z', 'status' => null, 'revoked_at' => null, 'revision' => 3],
        ['name' => 'workstation', 'stale' => true, 'listening' => false, 'protocol' => null, 'open_jobs' => 0, 'last_seen_at' => null, 'status' => null, 'revoked_at' => null, 'revision' => 2],
    ]])]);

    $this->get(route('fleet'))->assertOk()->assertSee('listening')->assertSee('stale')->assertDontSee('stopped');
});

it('reads only the Director, never this app\'s database', function () {
    fakeIntent([jobRecord(['state' => 'done'])], [runRecord()], [
        director('intents?*') => Http::response(['intents' => [intentRecord()], 'jobs' => [jobRecord()]]),
        director('runs/run_1') => Http::response(runRecord()),
        director('nodes') => Http::response(['nodes' => []]),
    ]);
    // No database is configured (phpunit.xml names a connection that does not exist), so any read of one fails.

    foreach ([route('intents.index'), route('intents.show', 'int_1'), route('runs.show', 'run_1'), route('fleet'), route('intents.create'), route('connect')] as $url) {
        $this->get($url)->assertOk();
    }
});

it('makes a fresh key for every user action', function () {
    expect(DirectorClient::newKey())->toMatch('/^[A-Za-z0-9.:_-]{1,128}$/')->not->toBe(DirectorClient::newKey());
});
