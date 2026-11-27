<?php

use App\Native\ReleaseApprovals;

it('lists smoke checks and skips ones missing the required name or ok', function () {
    $outcome = ['smoke' => ['ok' => false, 'checks' => [
        ['name' => 'director', 'ok' => true],
        ['name' => 'gateway', 'ok' => false, 'reason' => 'no answer'],
        ['name' => 'harness'],
        ['name' => 'tools', 'ok' => 'yes'],
        ['ok' => true],
    ]]];

    expect(ReleaseApprovals::checks($outcome))->toBe([
        'director: passed',
        'gateway: failed (no answer)',
    ]);
});
