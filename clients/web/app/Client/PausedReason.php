<?php

namespace App\Client;

/**
 * Why the Director has paused a conversation's dispatch or a schedule's firings (`paused_reason`), in words. A record
 * from a Director without the field gives null here, and the page keeps its own guess.
 */
final class PausedReason
{
    /**
     * Words for a record's pause: '' when the Director says it is not paused, null when it does not say.
     *
     * @param  array<string, mixed>  $record
     */
    public static function words(array $record): ?string
    {
        if (! array_key_exists('paused_reason', $record)) {
            return null;
        }

        $reason = $record['paused_reason'];
        if (! is_string($reason) || $reason === '') {
            return '';
        }

        return match ($reason) {
            'sponsor_ineligible' => __('Paused: the member responsible can no longer run work. An owner or admin can take it over.'),
            'member_disabled' => __('Paused: the member responsible is disabled. An owner or admin can take it over.'),
            default => __('Paused: :reason.', ['reason' => str_replace('_', ' ', $reason)]),
        };
    }
}
