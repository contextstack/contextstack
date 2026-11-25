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
    'removed and added lines that look like file headers' => [
        "diff --git a/q.sql b/q.sql\n--- a/q.sql\n+++ b/q.sql\n@@ -1,3 +1,2 @@\n-- old comment\n+++ counter\n select 1;\n-- kept\n"
            ."diff --git a/r.lua b/r.lua\n--- a/r.lua\n+++ b/r.lua\n@@ -1 +1 @@\n--- x\n+-- y\n\\ No newline at end of file\n",
        ['files' => ['q.sql', 'r.lua'], 'added' => 2, 'removed' => 3],
    ],
]);
