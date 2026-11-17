<?php

use App\Client\RunEvidence;

it('counts distinct paths and changed lines in diffs', function (string $diff, array $expected) {
    expect(RunEvidence::diffStat($diff))->toBe($expected);
})->with([
    'modification' => [
        "--- a/x\n+++ b/x\n-old\n+new\n",
        ['files' => ['x'], 'added' => 1, 'removed' => 1],
    ],
    'addition' => [
        "--- /dev/null\n+++ b/new\n+new\n",
        ['files' => ['new'], 'added' => 1, 'removed' => 0],
    ],
    'deletion' => [
        "--- a/old\n+++ /dev/null\n-old\n",
        ['files' => ['old'], 'added' => 0, 'removed' => 1],
    ],
    'rename with modification' => [
        "diff --git a/old b/new\nrename from old\nrename to new\n--- a/old\n+++ b/new\n-old\n+new\n",
        ['files' => ['new', 'old'], 'added' => 1, 'removed' => 1],
    ],
]);
