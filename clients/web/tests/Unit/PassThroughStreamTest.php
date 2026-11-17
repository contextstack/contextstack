<?php

use App\Client\PassThroughStream;

it('asks an idle receiver whether it is still there with a comment line, only between events', function () {
    $sent = [];
    $gone = false;
    $sink = new PassThroughStream(function (string $chunk) use (&$sent, &$gone): bool {
        $sent[] = $chunk;

        return ! $gone;
    });
    $sink->status = 200;

    $sink->write("event: change\ndata: {\"a\":");
    expect($sink->stillWanted(0.0))->toBeTrue()->and($sent)->toHaveCount(1);

    $sink->write("1}\n\n");
    expect($sink->stillWanted(60.0))->toBeTrue()->and($sent)->toHaveCount(2);
    expect($sink->stillWanted(0.0))->toBeTrue()->and($sent[2])->toBe(":\n\n");

    $gone = true;
    expect($sink->stillWanted(0.0))->toBeFalse();
});

it('does not probe before the answer is known to be a 200', function () {
    $sent = [];
    $sink = new PassThroughStream(function (string $chunk) use (&$sent): bool {
        $sent[] = $chunk;

        return false;
    });

    expect($sink->stillWanted(0.0))->toBeTrue()->and($sent)->toBe([]);
});
