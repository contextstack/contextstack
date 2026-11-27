<?php

namespace App\Native;

/**
 * The two links a ContextStack app shows as QR codes and copyable text: a pairing link for another device of the same
 * member, contextstack://pair?url=URL&code=CODE, and an invite link for a new member, contextstack://invite?url=URL&code=CODE
 * (docs/design/teams-step2.md). Both carry the Director's origin and a one-time code; neither is ever written down.
 */
final class PairingLink
{
    public const string Pair = 'pair';

    public const string Invite = 'invite';

    /**
     * @return array{kind: 'pair'|'invite', url: string, code: string}|null null unless it is a pairing or invite link
     *                                                                      with an http(s) URL and a code
     */
    public static function parse(string $link): ?array
    {
        $parts = parse_url(trim($link));
        if (! is_array($parts) || ($parts['scheme'] ?? null) !== 'contextstack') {
            return null;
        }

        $kind = match ($parts['host'] ?? null) {
            self::Pair => self::Pair,
            self::Invite => self::Invite,
            default => null,
        };
        if ($kind === null) {
            return null;
        }

        parse_str($parts['query'] ?? '', $query);

        return self::fields($kind, $query['url'] ?? null, $query['code'] ?? null);
    }

    /**
     * The same checks for the parts of a link that arrived separately (a deep link's query on the /pair or /invite
     * route).
     *
     * @param  'pair'|'invite'  $kind
     * @return array{kind: 'pair'|'invite', url: string, code: string}|null
     */
    public static function fields(string $kind, mixed $url, mixed $code): ?array
    {
        $url = is_string($url) ? trim($url) : '';
        $code = is_string($code) ? trim($code) : '';

        if ($code === '' || preg_match('#^https?://[^\s/]+#', $url) !== 1) {
            return null;
        }

        return ['kind' => $kind, 'url' => rtrim($url, '/'), 'code' => $code];
    }
}
