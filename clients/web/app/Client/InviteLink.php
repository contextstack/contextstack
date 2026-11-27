<?php

namespace App\Client;

/**
 * The app link an invite travels as: contextstack://invite?url=DIRECTOR&code=CODE (CLIENT-API.md §2). Pairing an
 * existing member's device keeps its own contextstack://pair link (QrCode::pairingLink). The Director writes the
 * link in its invite answer; this reads one back when a person pastes it on Connect.
 */
class InviteLink
{
    /**
     * The Director URL and code in a pasted invite link, or null when the text is not one (a bare code).
     *
     * @return array{url: string, code: string}|null
     */
    public static function parse(string $text): ?array
    {
        $text = trim($text);
        if (! str_starts_with(strtolower($text), 'contextstack://invite?')) {
            return null;
        }

        parse_str((string) parse_url($text, PHP_URL_QUERY), $query);
        $url = $query['url'] ?? null;
        $code = $query['code'] ?? null;

        if (! is_string($url) || ! is_string($code) || $url === '' || $code === '') {
            return null;
        }

        return ['url' => $url, 'code' => $code];
    }

    public static function make(string $url, string $code): string
    {
        return 'contextstack://invite?'.http_build_query(['url' => $url, 'code' => $code], '', '&', PHP_QUERY_RFC3986);
    }
}
