<?php

namespace App\Native;

use Carbon\CarbonImmutable;
use Throwable;

/**
 * Director times (RFC 3339, UTC) in words a person reads at a glance: how long ago, or how long until. The phone's
 * PHP runs in UTC and does not know the device's time zone, so a relative time is the one that is always right.
 */
final class When
{
    public static function relative(mixed $at, string $missing = '—'): string
    {
        if (! is_string($at) || $at === '') {
            return $missing;
        }

        try {
            return CarbonImmutable::parse($at)->diffForHumans();
        } catch (Throwable) {
            return $at;
        }
    }
}
