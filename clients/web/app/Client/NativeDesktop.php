<?php

declare(strict_types=1);

namespace App\Client;

final class NativeDesktop
{
    public static function isRunning(): bool
    {
        return (bool) config('nativephp-internal.running');
    }
}
