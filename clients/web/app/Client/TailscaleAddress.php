<?php

namespace App\Client;

use Illuminate\Support\Facades\Process;

/**
 * This machine's Tailscale IPv4 address, for telling another device where the Director is. The desktop app's PATH is
 * minimal, so the usual install locations are tried directly. Null when Tailscale is not installed or not up.
 */
class TailscaleAddress
{
    /** @var list<string> */
    public const array Candidates = [
        '/usr/local/bin/tailscale',
        '/opt/homebrew/bin/tailscale',
        '/Applications/Tailscale.app/Contents/MacOS/Tailscale',
        '/usr/bin/tailscale',
    ];

    public function ipv4(): ?string
    {
        foreach (self::Candidates as $binary) {
            if (! is_file($binary) || ! is_executable($binary)) {
                continue;
            }

            $result = Process::timeout(3)->run([$binary, 'ip', '-4']);
            $ip = trim(strtok($result->output(), "\n") ?: '');

            if ($result->successful() && filter_var($ip, FILTER_VALIDATE_IP, FILTER_FLAG_IPV4) !== false) {
                return $ip;
            }
        }

        return null;
    }
}
