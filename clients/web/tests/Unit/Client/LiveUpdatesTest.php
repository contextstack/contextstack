<?php

use App\Client\LiveUpdates;

it('applies a change only when its revision is higher than the record held', function () {
    $records = ['job:j1' => ['state' => 'accepted', 'revision' => 10]];

    expect(LiveUpdates::applyChange($records, ['kind' => 'job', 'id' => 'j1', 'revision' => 10, 'record' => ['state' => 'old', 'revision' => 10]]))->toBeNull()
        ->and(LiveUpdates::applyChange($records, ['kind' => 'job', 'id' => 'j1', 'revision' => 9, 'record' => ['state' => 'older', 'revision' => 9]]))->toBeNull()
        ->and(LiveUpdates::applyChange($records, ['kind' => 'job', 'id' => 'j1', 'revision' => 11, 'record' => ['state' => 'done', 'revision' => 11]])['job:j1']['state'])->toBe('done');
});

it('applies a record not held yet, and keys records by kind as well as id', function () {
    $records = ['job:x' => ['revision' => 50]];

    $next = LiveUpdates::applyChange($records, ['kind' => 'run', 'id' => 'x', 'revision' => 3, 'record' => ['revision' => 3]]);

    expect($next)->toHaveKeys(['job:x', 'run:x']);
});

it('ignores a malformed change', function () {
    expect(LiveUpdates::applyChange([], ['kind' => 'job', 'id' => 'j1', 'revision' => '7', 'record' => []]))->toBeNull()
        ->and(LiveUpdates::applyChange([], ['kind' => 'job', 'revision' => 7, 'record' => []]))->toBeNull();
});

it('appends live output in seq order above the last seq shown, once each', function () {
    $output = ['seq' => 2, 'text' => 'Hel'];

    $output = LiveUpdates::appendOutput($output, [
        ['seq' => 4, 'kind' => 'delta', 'payload' => ['text' => '!']],
        ['seq' => 2, 'kind' => 'delta', 'payload' => ['text' => 'Hel']],
        ['seq' => 3, 'kind' => 'delta', 'payload' => ['text' => 'lo']],
        ['seq' => 5, 'kind' => 'stop', 'payload' => ['reason' => 'end_turn']],
    ]);

    expect($output)->toBe(['seq' => 5, 'text' => 'Hello!']);
});

it('holds nothing for a run whose first catch-up failed, so the next event tries again instead of leaving a gap', function () {
    expect(LiveUpdates::catchUp(null, fn (int $after): mixed => null))->toBeNull()
        ->and(LiveUpdates::catchUp(['seq' => 4, 'text' => 'kept'], fn (int $after): mixed => null))->toBe(['seq' => 4, 'text' => 'kept']);
});

it('catches up page by page, and stops after a bounded number of pages', function () {
    $asked = [];
    $fetch = function (int $after) use (&$asked): array {
        $asked[] = $after;

        return ['events' => array_map(fn (int $n): array => ['seq' => $after + $n, 'kind' => 'delta', 'payload' => ['text' => 'x']], range(1, 1000))];
    };

    $caught = LiveUpdates::catchUp(null, $fetch);

    expect($asked)->toHaveCount(20)->and($caught['seq'])->toBe(20000);
});

it('keeps only the end of a long run\'s text', function () {
    $tail = LiveUpdates::tail(['seq' => 9, 'text' => str_repeat('a', 100).'END'], 10);

    expect($tail)->toBe(['seq' => 9, 'text' => '…aaaaaaaEND']);
});
