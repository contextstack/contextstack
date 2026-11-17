<?php

use App\Client\RunEvidence;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * @return array<string, mixed>
 */
function laneIntentRecord(array $over = []): array
{
    return [...[
        'intent_id' => 'int_lane',
        'title' => 'Fix the widget',
        'node' => 'laptop',
        'spec' => 'spec.yaml',
        'task' => 'tasks/ui-lanes',
        'trials' => 1,
        'meta' => ['source' => 'client-web-lane', 'repo' => 'contextstack', 'lane' => 'ui-lanes', 'model' => 'codex'],
        'created_at' => '2026-11-12T10:00:00Z',
        'revision' => 1,
    ], ...$over];
}

/**
 * @return array<string, mixed>
 */
function laneJobRecord(array $over = []): array
{
    return [...[
        'job_id' => 'job_lane',
        'intent_id' => 'int_lane',
        'node' => 'laptop',
        'state' => 'accepted',
        'created_at' => '2026-11-12T10:00:05Z',
        'live_run_ids' => [],
        'run_id' => null,
        'run_ids' => [],
        'revision' => 2,
    ], ...$over];
}

function laneModelsCatalog(): array
{
    return [
        'models' => [
            ['name' => 'codex', 'label' => 'Codex (subscription)', 'available' => true],
        ],
        'default_model' => 'codex',
    ];
}

it('starts the Director\'s built-in coding lane with repo, lane and model, uploading nothing, and opens the lane page', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(laneModelsCatalog()),
        director('repos') => Http::response(['repos' => [['name' => 'contextstack']], 'repos_file' => '/cfg/repos.json']),
        director('intents') => Http::response(['intent' => laneIntentRecord(), 'jobs' => [laneJobRecord(['state' => 'offered'])], 'runs' => []], 201),
    ]);

    $page = Livewire::test('pages::client.lane-create')
        ->set('title', 'Fix the widget')
        ->set('goal', "Make it work.\n")
        ->set('repo', 'contextstack')
        ->set('lane', 'ui-lanes')
        ->set('model', 'codex');

    $key = $page->get('submitKey');
    $page->call('submit')->assertHasNoErrors()->assertRedirect(route('lanes.show', 'job_lane'));

    Http::assertSent(function (Request $request) use ($key) {
        if ($request->method() !== 'POST' || $request->url() !== director('intents')) {
            return false;
        }

        return $request->hasHeader('Idempotency-Key', $key) && $request['node'] === 'laptop' && ! isset($request['files'])
            && $request['lane'] === ['title' => 'Fix the widget', 'goal' => "Make it work.\n", 'model' => 'codex', 'repo' => 'contextstack', 'lane' => 'ui-lanes'];
    });
    Http::assertNotSent(fn (Request $request) => str_contains($request->url(), 'objects/'));
});

it('starts scratch work with neither repo nor lane name', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(laneModelsCatalog()),
        director('repos') => Http::response(['repos' => [['name' => 'contextstack']]]),
        director('intents') => Http::response(['intent' => laneIntentRecord(), 'jobs' => [laneJobRecord(['state' => 'offered'])], 'runs' => []], 201),
    ]);

    Livewire::test('pages::client.lane-create')->set('title', 'Try an idea')->set('goal', 'Sketch it.')->set('scratch', true)
        ->call('submit')->assertHasNoErrors();

    Http::assertSent(fn (Request $request) => $request->url() === director('intents')
        && ! array_key_exists('repo', $request['lane']) && ! array_key_exists('lane', $request['lane']));
});

it('lists lane jobs and shows the lane detail with live output and result', function () {
    $diff = "--- a/README.md\n+++ b/README.md\n+hello\n";
    $transcript = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done.\"}]}}\n";
    Http::fake([
        director('intents?*') => Http::response(['intents' => [laneIntentRecord()], 'jobs' => [laneJobRecord(['state' => 'done', 'run_id' => 'run_lane', 'run_ids' => ['run_lane']])]]),
        director('jobs/job_lane') => Http::response(laneJobRecord(['state' => 'done', 'run_id' => 'run_lane', 'run_ids' => ['run_lane']])),
        director('intents/int_lane') => Http::response([
            'intent' => laneIntentRecord(),
            'jobs' => [laneJobRecord(['state' => 'done', 'run_id' => 'run_lane'])],
            'runs' => [[
                'run_id' => 'run_lane',
                'job_id' => 'job_lane',
                'intent_id' => 'int_lane',
                'verdict' => 'pass',
                'files' => [
                    ['path' => 'evidence/candidate.diff'],
                    ['path' => 'transcript.md'],
                ],
                'revision' => 9,
            ]],
        ]),
        director('runs/run_lane/events*') => Http::response(['run_id' => 'run_lane', 'finished' => true, 'confirmed' => 0, 'events' => []]),
        'http://director.test/api/v1/client/runs/run_lane/files/evidence/candidate.diff' => Http::response($diff),
        'http://director.test/api/v1/client/runs/run_lane/files/transcript.md' => Http::response($transcript),
    ]);

    $this->get(route('lanes.index'))->assertOk()->assertSee('Fix the widget');

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertSee('Check verdict')
        ->assertSee('pass')
        ->assertSee('README.md')
        ->assertSee('Done.')
        ->assertSee("cs land 'contextstack' 'ui-lanes'");
});

it('approves a lane run and shows the land command', function () {
    Http::fake([
        director('jobs/job_lane') => Http::response(laneJobRecord(['state' => 'done', 'run_id' => 'run_lane', 'run_ids' => ['run_lane']])),
        director('intents/int_lane') => Http::response([
            'intent' => laneIntentRecord(),
            'jobs' => [laneJobRecord(['state' => 'done', 'run_id' => 'run_lane'])],
            'runs' => [['run_id' => 'run_lane', 'job_id' => 'job_lane', 'intent_id' => 'int_lane', 'verdict' => 'pass', 'files' => [], 'revision' => 9]],
        ]),
        director('runs/run_lane/events*') => Http::response(['events' => [], 'confirmed' => 0]),
        director('runs/run_lane/decision') => Http::response([
            'decision_id' => 'dec_1',
            'decision' => 'approve',
            'decided_at' => '2026-11-12T10:05:00Z',
            'run_id' => 'run_lane',
            'scope' => 'run_result',
            'authorises' => [],
        ], 201),
    ]);

    $page = Livewire::test('pages::client.lane', ['job' => 'job_lane']);
    $key = $page->get('decisionKey');
    $page->call('approve')->assertSee("cs land 'contextstack' 'ui-lanes'");

    Http::assertSent(fn (Request $request) => $request->method() === 'POST'
        && $request->url() === director('runs/run_lane/decision')
        && $request->hasHeader('Idempotency-Key', $key)
        && $request['decision'] === 'approve');
});

it('requests cancel on a running lane', function () {
    Http::fake([
        director('jobs/job_lane') => Http::sequence()
            ->push(laneJobRecord())
            ->push(laneJobRecord(['cancel' => ['cancel_id' => 'can_1', 'state' => 'requested'], 'revision' => 5])),
        director('intents/int_lane') => Http::response(['intent' => laneIntentRecord(), 'jobs' => [laneJobRecord()], 'runs' => []]),
        director('jobs/job_lane/cancel') => Http::response(['cancel_id' => 'can_1', 'job_id' => 'job_lane', 'state' => 'requested', 'revision' => 20], 202),
        director('cancels/can_1') => Http::response(['cancel_id' => 'can_1', 'state' => 'requested', 'revision' => 20]),
        director('runs/*/events*') => Http::response(['events' => [], 'confirmed' => 0]),
    ]);

    $page = Livewire::test('pages::client.lane', ['job' => 'job_lane']);
    $key = $page->get('cancelKey');
    $page->set('cancelReason', 'wrong')->call('requestCancel');

    Http::assertSent(fn (Request $request) => $request->url() === director('jobs/job_lane/cancel')
        && $request->hasHeader('Idempotency-Key', $key));
});

it('summarises diff and transcript evidence like cs mcp', function () {
    $diff = RunEvidence::diffStat("--- a/x\n+++ b/x\n+one\n");
    expect($diff['files'])->toBe(['x'])->and($diff['added'])->toBe(1);

    $transcript = RunEvidence::finalMessage("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Hi\"}]}}\n");
    expect($transcript)->toBe('Hi');
});

it('shows the agent\'s last message, not its progress notes or a result that repeats them', function () {
    $event = fn (array $e): string => json_encode($e)."\n";
    $say = fn (string $text): string => $event(['type' => 'assistant', 'message' => ['content' => [['type' => 'text', 'text' => $text]]]]);
    $raw = $say('I will read the failing checks first.')
        .$event(['type' => 'tool_call', 'subtype' => 'started'])
        .$say("The four regressions are fixed.\n\nAll checks pass.")
        .$event(['type' => 'result', 'result' => 'I will read the failing checks first.The four regressions are fixed.']);

    expect(RunEvidence::finalMessage($raw))->toBe("The four regressions are fixed.\n\nAll checks pass.")
        ->and(RunEvidence::finalMessage($event(['type' => 'result', 'result' => 'Only a result.'])))->toBe('Only a result.');

    $long = RunEvidence::finalMessage($say(str_repeat("line of the report\n", 400)), 1000);
    expect($long)->toStartWith('…line of the report')->and(mb_strlen($long))->toBeLessThanOrEqual(1001);
});

it('withholds land commands for adversarial metadata', function (string $field, string $value) {
    $meta = laneIntentRecord()['meta'];
    $meta[$field] = $value;
    Http::fake([
        director('jobs/job_lane') => Http::response(laneJobRecord(['state' => 'done', 'run_id' => 'run_lane'])),
        director('intents/int_lane') => Http::response([
            'intent' => laneIntentRecord(['meta' => $meta]),
            'jobs' => [laneJobRecord(['state' => 'done', 'run_id' => 'run_lane'])],
            'runs' => [['run_id' => 'run_lane', 'job_id' => 'job_lane', 'intent_id' => 'int_lane', 'verdict' => 'pass', 'files' => [], 'revision' => 9]],
        ]),
        director('runs/run_lane/events*') => Http::response(['events' => [], 'confirmed' => 0]),
    ]);

    $page = Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertDontSee('data-test="land-command"', false);

    expect($page->instance()->landCommand())->toBeNull();
})->with([
    'lane shell separator' => ['lane', 'foo; touch /tmp/x'],
    'repo command substitution' => ['repo', '$(touch /tmp/x)'],
    'lane quote' => ['lane', "foo'bar"],
    'repo newline' => ['repo', "foo\n"],
    'lane whitespace' => ['lane', 'foo bar'],
    'empty repo' => ['repo', ''],
]);

/**
 * A finished lane whose job and run carry the given land record (CLIENT-API.md §6.4).
 *
 * @param  array<string, mixed>|null  $land
 */
function fakeLandedLane(?array $land): void
{
    $job = laneJobRecord(['state' => 'done', 'run_id' => 'run_lane', 'run_ids' => ['run_lane'], 'land' => $land, 'land_state' => $land['state'] ?? null]);
    $run = ['run_id' => 'run_lane', 'job_id' => 'job_lane', 'intent_id' => 'int_lane', 'node' => 'laptop', 'received_at' => '2026-11-13T21:50:00Z', 'verdict' => 'pass', 'files' => [],
        'decision' => null, 'land' => $land, 'revision' => 9];
    Http::fake([
        director('intents?*') => Http::response(['intents' => [laneIntentRecord()], 'jobs' => [$job]]),
        director('jobs/job_lane') => Http::response($job),
        director('intents/int_lane') => Http::response(['intent' => laneIntentRecord(), 'jobs' => [$job], 'runs' => [$run]]),
        director('runs/run_lane/events*') => Http::response(['events' => [], 'confirmed' => 0]),
        director('runs/run_lane') => Http::response($run),
    ]);
}

/**
 * @return array<string, mixed>
 */
function landRecord(array $over = []): array
{
    return [...['land_id' => 'lnd_1', 'state' => 'landed', 'repo' => 'contextstack', 'lane' => 'lane/ui-lanes', 'main' => 'main',
        'commit' => '01b1e77e6a3f0c2d', 'reason' => null, 'landed_by' => 'owner', 'landed_at' => '2026-11-13T22:00:00Z'], ...$over];
}

it('marks a landed lane on the Task lanes list with its commit', function () {
    fakeLandedLane(landRecord());

    $page = Livewire::test('pages::client.lanes')
        ->assertSeeText('landed')
        ->assertSeeText('01b1e77e6');

    expect($page->get('records')['job:job_lane']['land'])->toBe(['state' => 'landed', 'commit' => '01b1e77e6a3f0c2d', 'reason' => null]);
});

it('shows where a landed lane went and no longer offers to judge or land it', function () {
    fakeLandedLane(landRecord());

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertSee('data-test="lane-landed"', false)
        ->assertSeeText('Landed on main')
        ->assertSeeText('01b1e77e6a3f0c2d')
        ->assertDontSee('data-test="lane-decision"', false)
        ->assertDontSee('data-test="land-command"', false);
});

it('says why a land failed and still offers the command', function () {
    fakeLandedLane(landRecord(['state' => 'failed', 'commit' => null, 'reason' => 'check failed: harness line cap']));

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertSeeText('Landing failed')
        ->assertSeeText('check failed: harness line cap')
        ->assertSee("cs land 'contextstack' 'ui-lanes'")
        ->assertSee('data-test="lane-decision"', false)
        ->assertDontSee('data-test="lane-landed"', false);

    Livewire::test('pages::client.lanes')->assertSeeText('land failed');
});

it('turns a lane page to landed when the land arrives on the stream', function () {
    fakeLandedLane(null);
    $page = Livewire::test('pages::client.lane', ['job' => 'job_lane'])->assertSee('data-test="land-command"', false);

    $page->call('applyStream', [['kind' => 'job', 'id' => 'job_lane', 'revision' => 20,
        'record' => laneJobRecord(['state' => 'done', 'run_id' => 'run_lane', 'run_ids' => ['run_lane'], 'land' => landRecord(), 'land_state' => 'landed', 'revision' => 20])]]);

    $page->assertSee('data-test="lane-landed"', false)->assertDontSee('data-test="land-command"', false);
});

it('does not offer approve or reject on a run that landed undecided', function () {
    fakeLandedLane(landRecord());

    Livewire::test('pages::client.run', ['run' => 'run_lane'])
        ->assertSeeText('landed on main as 01b1e77e6 without a decision')
        ->assertDontSeeText('Approve');
});

it('names the branch, not the checkout folder, when a land record holds a path', function () {
    fakeLandedLane(landRecord(['main' => '/Users/example/src/contextstack/main']));

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertSeeText('Landed on main')
        ->assertDontSeeText('/Users/example');
});

/**
 * A lane retried once: the first job failed, the retry (another job on the same intent) is running.
 *
 * @return array{intents: list<array<string, mixed>>, jobs: list<array<string, mixed>>}
 */
function retriedLaneAnswer(): array
{
    return [
        'intents' => [laneIntentRecord()],
        'jobs' => [
            laneJobRecord(['job_id' => 'job_retry', 'state' => 'accepted', 'created_at' => '2026-11-12T11:00:00Z', 'revision' => 6]),
            laneJobRecord(['job_id' => 'job_lane', 'state' => 'error', 'created_at' => '2026-11-12T10:00:05Z', 'revision' => 4]),
        ],
    ];
}

it('lists a retried lane once, as its newest attempt', function () {
    Http::fake([director('intents?*') => Http::response(retriedLaneAnswer())]);

    $page = Livewire::test('pages::client.lanes');

    $rows = $page->instance()->rows();
    expect($rows)->toHaveCount(1)
        ->and($rows[0]['job']['job_id'])->toBe('job_retry')
        ->and(array_column($rows[0]['attempts'], 'job_id'))->toBe(['job_lane', 'job_retry']);
    $page->assertSeeText('attempt 2')->assertSee(route('lanes.show', 'job_retry'))->assertDontSee(route('lanes.show', 'job_lane'));
});

it('counts lanes, not retry jobs, when deciding whether there is more to show', function () {
    $answer = ['intents' => [], 'jobs' => []];
    foreach (range(1, 50) as $n) {
        $answer['intents'][] = laneIntentRecord(['intent_id' => "int_{$n}", 'title' => "Lane {$n}"]);
        $answer['jobs'][] = laneJobRecord(['job_id' => "job_{$n}", 'intent_id' => "int_{$n}", 'created_at' => sprintf('2026-11-12T10:%02d:00Z', $n)]);
        $answer['jobs'][] = laneJobRecord(['job_id' => "job_{$n}_retry", 'intent_id' => "int_{$n}", 'created_at' => sprintf('2026-11-12T11:%02d:00Z', $n)]);
    }
    Http::fake([director('intents?*') => Http::response($answer)]);

    $page = Livewire::test('pages::client.lanes');

    expect($page->get('more'))->toBeFalse()
        ->and($page->instance()->rows())->toHaveCount(50);
});

it('links every attempt of a retried lane from each attempt\'s page', function () {
    $answer = retriedLaneAnswer();
    Http::fake([
        director('jobs/job_lane') => Http::response($answer['jobs'][1]),
        director('intents/int_lane') => Http::response(['intent' => laneIntentRecord(), 'jobs' => $answer['jobs'], 'runs' => []]),
    ]);

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertSee('data-test="lane-attempts"', false)
        ->assertSeeText('#1 error')
        ->assertSee(route('lanes.show', 'job_retry'))
        ->assertSeeText('#2 accepted')
        ->assertSet('attempts', [['job_id' => 'job_lane', 'state' => 'error'], ['job_id' => 'job_retry', 'state' => 'accepted']]);
});

it('shows no attempts line on a lane that was never retried', function () {
    Http::fake([
        director('jobs/job_lane') => Http::response(laneJobRecord()),
        director('intents/int_lane') => Http::response(['intent' => laneIntentRecord(), 'jobs' => [laneJobRecord()], 'runs' => []]),
    ]);

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertDontSee('data-test="lane-attempts"', false)
        ->assertSet('attempts', []);
});

it('suggests a lane name from the title, keeping every letter', function () {
    Http::fake([director('*') => Http::response(['nodes' => [], 'models' => [], 'repos' => []])]);

    Livewire::test('pages::client.lane-create')->set('title', 'Add Dark Mode')->assertSet('lane', 'add-dark-mode');
});

it('does not take a refused transcript or diff for the run\'s evidence, and reads it once it is given', function () {
    $job = laneJobRecord(['state' => 'done', 'run_id' => 'run_lane', 'run_ids' => ['run_lane']]);
    $run = ['run_id' => 'run_lane', 'job_id' => 'job_lane', 'intent_id' => 'int_lane', 'verdict' => 'pass', 'revision' => 9,
        'files' => [['path' => 'evidence/candidate.diff'], ['path' => 'transcript.md']]];
    Http::fake([
        director('jobs/job_lane') => Http::response($job),
        director('intents/int_lane') => Http::response(['intent' => laneIntentRecord(), 'jobs' => [$job], 'runs' => [$run]]),
        'http://director.test/api/v1/client/runs/run_lane/files/*' => Http::sequence()
            ->push(['error' => 'Bad Gateway', 'code' => 'unavailable'], 502)
            ->push(['error' => 'Bad Gateway', 'code' => 'unavailable'], 502)
            ->push("--- a/x\n+++ b/x\n+hi\n")
            ->push("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done.\"}]}}\n"),
    ]);

    $page = Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertSet('diff', null)
        ->assertSet('lastMessage', '')
        ->assertSet('transcriptRead', false);

    $page->call('refetch')
        ->assertSet('lastMessage', 'Done.')
        ->assertSet('transcriptRead', true);
    expect($page->get('diff')['files'])->toBe(['x']);
});
