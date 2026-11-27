<?php

use App\Native\ActionApprovals;
use App\Native\ReleaseApprovals;

it('words an approval that expired when its job ended', function () {
    expect(ActionApprovals::status(['status' => 'expired']))->toBe('Expired: its job ended before it was decided');
});

it('mirrors the Director rule for which release requests an owner may clear', function (array $approval, bool $clearable) {
    expect(ReleaseApprovals::clearable(['kind' => 'install'] + $approval))->toBe($clearable);
})->with([
    'pending' => [['status' => 'pending'], true],
    'needs owner' => [['status' => 'needs_owner'], true],
    'approved, not reported' => [['status' => 'approved', 'outcome' => ['status' => 'unknown']], true],
    'interrupted' => [['status' => 'approved', 'outcome' => ['status' => 'interrupted']], true],
    'interrupted and cleared' => [['status' => 'approved', 'outcome' => ['status' => 'interrupted', 'cleared' => true]], false],
    'rollback waiting on jobs' => [['status' => 'approved', 'outcome' => ['status' => 'rollback_failed', 'waiting_jobs' => 2]], true],
    'installed' => [['status' => 'approved', 'outcome' => ['status' => 'installed']], false],
    'denied' => [['status' => 'denied'], false],
]);

it('names a release request and needs its pin before it can be approved', function () {
    expect(ActionApprovals::headline(['kind' => 'install']))->toBe('Install local main')
        ->and(ActionApprovals::headline(['kind' => 'install', 'bundle' => 'cs-1']))->toBe('Install bundle cs-1')
        ->and(ActionApprovals::headline(['kind' => 'rollback']))->toBe('Roll back to the previous release')
        ->and(ActionApprovals::decidable(['kind' => 'install', 'status' => 'needs_owner', 'commit' => 'a']))->toBeFalse()
        ->and(ActionApprovals::decidable(['kind' => 'install', 'status' => 'needs_owner', 'commit' => 'a', 'target_hash' => 'b']))->toBeTrue()
        ->and(ReleaseApprovals::schema(null))->toBe('No local Director')
        ->and(ReleaseApprovals::outcomeWords(['status' => 'approved', 'outcome' => ['status' => 'rollback_failed', 'waiting_jobs' => 1]]))
        ->toContain('waiting on 1 job.');
});
