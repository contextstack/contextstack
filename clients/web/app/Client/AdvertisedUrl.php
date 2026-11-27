<?php

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;

/**
 * The owner-set address other devices reach this Director at (`GET`/`PUT advertised-url`, CLIENT-API.md §2,
 * manage_org_settings). When set, the Director writes it into every invite and pairing link. When unset, a desktop on
 * the Director's own machine (a loopback connection) tells the Director its Tailscale address instead with
 * `X-ContextStack-Invite-Origin` (TailscaleAddress), which the Director honours only from loopback.
 */
class AdvertisedUrl
{
    public function __construct(private readonly Identity $identity) {}

    /**
     * The advertised URL, or null when it is unset, this caller may not read it (only owners can), or the Director
     * is from before the setting (404) or cannot be reached. Null means "send the fallback header": the Director still
     * prefers an advertised URL it has, so a header sent by a caller that could not read the setting is ignored.
     */
    public function read(): ?string
    {
        if ($this->identity->legacy() || ! $this->identity->can('manage_org_settings')) {
            return null;
        }

        try {
            $answer = app(DirectorClient::class)->find('advertised-url');
        } catch (DirectorRefused|DirectorUnavailable|NotConnected) {
            return null;
        }

        $url = $answer['url'] ?? null;

        return is_string($url) && $url !== '' ? $url : null;
    }

    /**
     * The link to show for a just-issued invite or pairing code: the Director's own `link` when it sent a
     * contextstack://{kind} link another device can use, else one built from $address ('' when there is no address
     * either). A link naming a loopback address (a Director with no advertised URL that got no usable origin) is never
     * shown: no other device reaches it.
     */
    public static function link(string $kind, mixed $directorLink, string $address, string $code): string
    {
        if (is_string($directorLink) && str_starts_with(strtolower($directorLink), 'contextstack://'.$kind.'?')) {
            $host = parse_url(self::address($directorLink), PHP_URL_HOST);
            if (is_string($host) && $host !== '' && ! in_array(strtolower($host), ['127.0.0.1', 'localhost', '::1', '[::1]'], true)) {
                return $directorLink;
            }
        }

        if ($address === '' || $code === '') {
            return '';
        }

        return $kind === 'invite' ? InviteLink::make($address, $code) : QrCode::pairingLink($address, $code);
    }

    /**
     * The Director address a contextstack:// link carries ('' when it carries none).
     */
    public static function address(string $link): string
    {
        parse_str((string) parse_url($link, PHP_URL_QUERY), $query);
        $url = $query['url'] ?? null;

        return is_string($url) ? $url : '';
    }
}
