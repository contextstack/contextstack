<?php

declare(strict_types=1);

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;

/**
 * Whether the desktop app can reach the local Director, using the same credential and HTTP path as the rest of
 * client-web.
 */
final class DesktopConnection
{
    public function __construct(private readonly ConnectionStore $connections) {}

    public function isReady(): bool
    {
        return $this->status() === DesktopConnectionStatus::Ready;
    }

    public function status(): DesktopConnectionStatus
    {
        if (! $this->connections->hasLocalCredential()) {
            return DesktopConnectionStatus::NotSetUp;
        }

        try {
            app(DirectorClient::class)->get('me');
        } catch (DirectorUnavailable) {
            return DesktopConnectionStatus::NotRunning;
        } catch (NotConnected) {
            return DesktopConnectionStatus::NotSetUp;
        } catch (DirectorRefused $refused) {
            // A credential the Director rejects is as good as none: this Mac needs `cs init` again.
            return in_array($refused->status, [401, 403], true)
                ? DesktopConnectionStatus::NotSetUp
                : DesktopConnectionStatus::Ready;
        }

        return DesktopConnectionStatus::Ready;
    }
}
