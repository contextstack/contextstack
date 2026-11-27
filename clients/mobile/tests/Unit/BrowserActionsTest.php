<?php

use App\Native\BrowserActions;

it('reads the site result from the approval record outcome', function (array $outcome, string $expected) {
    $text = BrowserActions::outcome(['status' => 'approved', 'outcome' => $outcome]);

    expect($text)->toStartWith('Approved for one submission.')->toContain($expected);
})->with([
    'submitted' => [['status' => 'submitted'], 'The site accepted the submission.'],
    'legacy succeeded' => [['status' => 'succeeded'], 'The site accepted the submission.'],
    'submission unknown' => [['status' => 'submission_unknown'], 'Check the site before trying again.'],
    'refused with reason' => [['status' => 'refused', 'reason' => 'digest mismatch'], 'refused before sending anything: digest mismatch. It will be proposed again on the next run.'],
    'refused without reason' => [['status' => 'refused'], 'refused before sending anything. It will be proposed again on the next run.'],
]);

it('adds nothing about the site when the outcome is missing or unrecognised', function (mixed $outcome) {
    $text = BrowserActions::outcome(['status' => 'approved', 'outcome' => $outcome]);

    expect($text)->not->toContain('The site')->not->toContain('refused');
})->with([
    'missing' => [null],
    'not an object' => ['submitted'],
    'unknown status' => [['status' => 'mystery']],
]);
