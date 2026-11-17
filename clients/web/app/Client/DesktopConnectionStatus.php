<?php

declare(strict_types=1);

namespace App\Client;

enum DesktopConnectionStatus: string
{
    case NotSetUp = 'not-set-up';
    case NotRunning = 'not-running';
    case Ready = 'ready';
}
