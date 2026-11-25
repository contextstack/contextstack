<?php

use App\Native\ActionApprovals;

it('words an approval that expired when its job ended', function () {
    expect(ActionApprovals::status(['status' => 'expired']))->toBe('Expired: its job ended before it was decided');
});
