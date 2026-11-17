<?php

use App\Client\ListRecords;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * @return array<string, mixed>
 */
function listIntentRecord(array $over = []): array
{
    return [...['intent_id' => 'int_1', 'title' => 'say hello', 'node' => 'laptop', 'task' => 'tasks/hello', 'meta' => [],
        'created_at' => '2026-11-10T10:00:00Z', 'job_ids' => ['job_1'], 'revision' => 5], ...$over];
}

/**
 * @return array<string, mixed>
 */
function listJobRecord(array $over = []): array
{
    return [...['job_id' => 'job_1', 'intent_id' => 'int_1', 'node' => 'laptop', 'state' => 'accepted', 'created_at' => '2026-11-10T10:00:00Z',
        'cancel' => null, 'revision' => 6], ...$over];
}

/**
 * A Director intents answer shaped like a busy machine's: many intents and jobs, each job carrying the heavy fields
 * (terminal row, file list, budget) the list pages never show.
 *
 * @return array{intents: list<array<string, mixed>>, jobs: list<array<string, mixed>>}
 */
function busyIntentsAnswer(int $count): array
{
    $intents = [];
    $jobs = [];
    for ($i = 1; $i <= $count; $i++) {
        $intents[] = listIntentRecord([
            'intent_id' => "int_$i", 'title' => "task $i", 'job_ids' => ["job_$i"],
            'bundle_sha256' => str_repeat('c', 64), 'spec' => str_repeat('s', 500),
            'meta' => $i % 2 === 0 ? ['source' => 'cs-mcp', 'repo' => 'contextstack', 'lane' => "lane-$i", 'agent' => 'coder', 'notes' => str_repeat('n', 500)] : ['notes' => str_repeat('n', 500)],
        ]);
        $jobs[] = listJobRecord([
            'job_id' => "job_$i", 'intent_id' => "int_$i", 'state' => 'done',
            'terminal_row' => ['summary' => str_repeat('t', 2000)],
            'files' => array_fill(0, 10, ['path' => 'evidence/file.txt', 'sha256' => str_repeat('a', 64), 'bytes' => 10]),
            'budget' => ['turns' => 40, 'tool_calls' => 200, 'usd' => 5, 'seconds' => 3600, 'spent' => str_repeat('b', 300)],
        ]);
    }

    return ['intents' => $intents, 'jobs' => $jobs];
}

it('holds only the fields the Tasks list shows, so a long history stays well under the request limit', function () {
    Http::fake([director('intents?*') => Http::response(busyIntentsAnswer(400))]);

    $page = Livewire::test('pages::client.intents')->assertSee('task 1')->assertSee('done');

    $records = $page->get('records');
    expect($records)->toHaveCount(100)
        ->and(strlen((string) json_encode($records)))->toBeLessThan(250_000)
        ->and(array_keys($records['job:job_1']))->toBe(['job_id', 'intent_id', 'revision', 'created_at', 'state', 'cancel', 'land'])
        ->and($records['intent:int_2']['meta'])->toBe(['source' => 'cs-mcp', 'repo' => 'contextstack', 'lane' => 'lane-2']);
});

it('shortens a long task shown in place of a missing title', function () {
    $task = str_repeat('word ', 200);
    Http::fake([director('intents?*') => Http::response(['intents' => [listIntentRecord(['title' => null, 'task' => $task])], 'jobs' => []])]);

    $held = Livewire::test('pages::client.intents')->get('records')['intent:int_1'];

    expect(mb_strlen($held['task']))->toBeLessThanOrEqual(ListRecords::TASK_LIMIT + 3)
        ->and($held['task'])->toEndWith('...');
});

it('slims a stream change the same way and still applies it only when newer', function () {
    Http::fake([director('intents?*') => Http::response(['intents' => [listIntentRecord()], 'jobs' => [listJobRecord()]])]);
    $page = Livewire::test('pages::client.intents');

    $page->call('applyStream', [['kind' => 'job', 'id' => 'job_1', 'revision' => 12,
        'record' => listJobRecord(['state' => 'done', 'revision' => 12, 'terminal_row' => ['summary' => 'big'], 'cancel' => ['state' => 'requested', 'by' => 'owner']])]]);
    $held = $page->get('records')['job:job_1'];
    expect($held['state'])->toBe('done')
        ->and($held)->not->toHaveKey('terminal_row')
        ->and($held['cancel'])->toBe(['state' => 'requested']);

    $page->call('applyStream', [['kind' => 'job', 'id' => 'job_1', 'revision' => 7, 'record' => listJobRecord(['state' => 'error', 'revision' => 7])]]);
    expect($page->get('records')['job:job_1']['state'])->toBe('done');
});

it('holds only lane intents and their jobs on the Task lanes page', function () {
    Http::fake([director('intents?*') => Http::response(busyIntentsAnswer(400))]);

    $page = Livewire::test('pages::client.lanes')->assertSee('task 2')->assertSee('contextstack/lane-2')->assertDontSee('task 1<');

    $records = $page->get('records');
    expect($records)->toHaveCount(100)
        ->and($records)->toHaveKey('intent:int_2')->not->toHaveKey('intent:int_1')
        ->and(strlen((string) json_encode($records)))->toBeLessThan(150_000);
});

it('shows the newest 50 tasks and lanes, and 50 more each time you ask', function (string $page, int $shown) {
    $answer = busyIntentsAnswer(120);
    foreach ($answer['intents'] as $i => $intent) {
        $answer['intents'][$i]['created_at'] = sprintf('2026-11-10T10:%02d:00Z', $i % 60).sprintf('%03d', $i);
    }
    foreach ($answer['jobs'] as $i => $job) {
        $answer['jobs'][$i]['created_at'] = $answer['intents'][$i]['created_at'];
    }
    Http::fake([director('intents?*') => Http::response($answer)]);

    $test = Livewire::test($page)->assertSee('data-test="show-more"', false);
    expect(count($test->get('records')))->toBe(100);

    $test->call('showMore');
    expect(count($test->get('records')))->toBe($shown);
})->with([
    'tasks' => ['pages::client.intents', 200],
    'lanes' => ['pages::client.lanes', 120],
]);

it('asks the Director only for what a list shows: one more than the window, and only lanes on the Task lanes page', function () {
    Http::fake([director('intents?*') => Http::response(['intents' => [], 'jobs' => []])]);

    Livewire::test('pages::client.intents')->call('showMore');
    Livewire::test('pages::client.lanes');

    $asked = Http::recorded()->map(fn (array $pair): string => (string) parse_url($pair[0]->url(), PHP_URL_QUERY))->all();
    expect($asked)->toBe(['limit=51', 'limit=101', 'lanes=1&limit=51']);
});

it('holds only what the conversation list shows, with a long question cut short', function () {
    $conversation = [
        'conversation_id' => 'con_1', 'title' => 'planning', 'node' => 'workstation', 'model' => 'codex', 'state' => 'idle', 'turns' => 3,
        'queued_turn_ids' => [], 'created_at' => '2026-11-14T10:00:00Z', 'revision' => 4, 'lead' => false,
        'budget' => ['state' => 'none', 'shown' => 'no budget set', 'usd' => null],
        'compaction' => ['context_window' => 32768, 'threshold_tokens' => 19000], 'spec_version' => str_repeat('c', 64),
        'attention' => ['question' => str_repeat('Which branch? ', 100), 'source_kind' => 'lead', 'set_at' => '2026-11-14T11:00:00Z'],
    ];
    Http::fake([director('conversations?*') => Http::response(['conversations' => [$conversation]])]);

    $held = Livewire::test('pages::client.conversations')->assertSeeText('planning')->get('records')['conversation:con_1'];

    expect($held)->not->toHaveKeys(['compaction', 'spec_version'])
        ->and($held['budget'])->toBe(['shown' => 'no budget set'])
        ->and(strlen($held['attention']['question']))->toBeLessThanOrEqual(203);
});
