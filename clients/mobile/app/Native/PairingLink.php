<?php

namespace App\Native;

/**
 * The pairing link the desktop app's Devices page shows as a QR code: contextstack://pair?url=URL&code=CODE.
 */
final class PairingLink
{
    /**
     * @return array{url: string, code: string}|null null unless it is a pairing link with an http(s) URL and a code
     */
    public static function parse(string $link): ?array
    {
        $parts = parse_url(trim($link));
        if (! is_array($parts) || ($parts['scheme'] ?? null) !== 'contextstack' || ($parts['host'] ?? null) !== 'pair') {
            return null;
        }

        parse_str($parts['query'] ?? '', $query);
        $url = is_string($query['url'] ?? null) ? trim($query['url']) : '';
        $code = is_string($query['code'] ?? null) ? trim($query['code']) : '';

        if ($code === '' || preg_match('#^https?://[^\s/]+#', $url) !== 1) {
            return null;
        }

        return ['url' => rtrim($url, '/'), 'code' => $code];
    }
}
