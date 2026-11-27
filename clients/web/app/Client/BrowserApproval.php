<?php

namespace App\Client;

use Carbon\CarbonImmutable;

/**
 * A browser action approval (CLIENT-API.md "Action approvals", docs/design/browser-action-approvals.md): an agent
 * driving a browser asks to `submit`, `post` or `buy` on an HTTPS origin. The extension captured the target, a
 * summary of the form values (masked entries carry only their length) and a screenshot when it proposed the action.
 * It goes straight to `needs_owner`; only a human owner decides it, before `expires_at`.
 */
final class BrowserApproval
{
    public const string Kind = 'browser_action';

    public const array Actions = ['submit', 'post', 'buy'];

    public static function is(mixed $approval): bool
    {
        return is_array($approval) && ($approval['kind'] ?? null) === self::Kind;
    }

    /**
     * Every well-formed `values_summary` entry, in the order sent, repeated names kept. An entry has a name, a hidden
     * flag and either a value or its length (masked). `excluded` (a per-load token left out of the digest) is shown
     * when the Director sends it. A malformed entry stays in place as an unreadable row so the count still matches.
     *
     * @param  array<string, mixed>  $approval
     * @return list<array{name: string, hidden: bool, value: ?string, length: ?int, excluded: bool}>
     */
    public static function fields(array $approval): array
    {
        $fields = [];
        foreach ((array) ($approval['values_summary'] ?? []) as $entry) {
            $entry = is_array($entry) ? $entry : [];
            $fields[] = [
                'name' => is_string($entry['name'] ?? null) ? $entry['name'] : '',
                'hidden' => ($entry['hidden'] ?? false) === true,
                'value' => is_string($entry['value'] ?? null) ? $entry['value'] : null,
                'length' => is_int($entry['length'] ?? null) && $entry['length'] >= 0 ? $entry['length'] : null,
                'excluded' => ($entry['excluded'] ?? false) === true,
            ];
        }

        return $fields;
    }

    /**
     * The number of masked or hidden fields when the Director flags them: `has_field_warning` always comes with
     * `masked_or_hidden_count`. Null when there is no warning.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function fieldWarning(array $approval): ?int
    {
        $count = $approval['masked_or_hidden_count'] ?? null;

        return ($approval['has_field_warning'] ?? false) === true && is_int($count) ? $count : null;
    }

    /**
     * Whether `expires_at` has passed by this device's clock. The Director still decides: a late decide or answer
     * gets a 409 and the record then reads `expired`.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function pastExpiry(array $approval): bool
    {
        $at = $approval['expires_at'] ?? null;
        if (! is_string($at) || $at === '') {
            return false;
        }
        try {
            return CarbonImmutable::parse($at)->isPast();
        } catch (\Throwable) {
            return false;
        }
    }

    /**
     * The screenshot digest, only when it is a lowercase sha256 (the desktop route accepts nothing else).
     *
     * @param  array<string, mixed>  $approval
     */
    public static function screenshotSha(array $approval): ?string
    {
        $sha = $approval['screenshot_sha'] ?? null;

        return is_string($sha) && self::validSha($sha) ? $sha : null;
    }

    public static function validSha(string $sha): bool
    {
        return preg_match('/^[0-9a-f]{64}$/D', $sha) === 1;
    }

    /**
     * Whether this device may approve or deny: the record's `actions.decide`, else the owner rule.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function decidable(array $approval): bool
    {
        return ($approval['status'] ?? null) === 'needs_owner' && ! self::pastExpiry($approval)
            && RecordActions::allows($approval, 'decide', fn (): bool => self::ownerHere());
    }

    /**
     * Whether this device may answer it as a needs-you item (go or no_go): the record's `actions.answer` only. A
     * record without it is decided with Approve and Deny instead.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function answerable(array $approval): bool
    {
        return ($approval['status'] ?? null) === 'needs_owner' && ! self::pastExpiry($approval)
            && RecordActions::allows($approval, 'answer', fn (): bool => false);
    }

    private static function ownerHere(): bool
    {
        $identity = app(Identity::class);

        return $identity->legacy() || (bool) ($identity->me()['owner'] ?? false);
    }
}
