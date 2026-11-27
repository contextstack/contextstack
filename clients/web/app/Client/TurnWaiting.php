<?php

namespace App\Client;

use Carbon\CarbonImmutable;
use Throwable;

/**
 * What a turn is waiting on, from the Director's derived `waiting` (CLIENT-API.md §11.2): its machine has not
 * picked the job up, or the job is held. '' when it waits on nothing, so the page shows its usual running words.
 */
final class TurnWaiting
{
    /**
     * @param  array<string, mixed>  $turn
     */
    public static function words(array $turn): string
    {
        $waiting = is_array($turn['waiting'] ?? null) ? $turn['waiting'] : [];
        $reason = $waiting['reason'] ?? null;

        if ($reason === 'node_not_pulled') {
            $node = is_string($waiting['node'] ?? null) && $waiting['node'] !== '' ? $waiting['node'] : __('its machine');
            $seen = self::ago($waiting['node_last_seen'] ?? null);

            return $seen !== ''
                ? __('Waiting for :node to pick this up (last heard from :seen)…', ['node' => $node, 'seen' => $seen])
                : __('Waiting for :node to pick this up…', ['node' => $node]);
        }

        if ($reason === 'held' && ($waiting['detail'] ?? null) === 'sponsor_ineligible') {
            return __('Paused: the member responsible for this conversation can no longer run work. An owner or admin can adopt it to resume.');
        }

        if ($reason === 'held') {
            $detail = is_string($waiting['detail'] ?? null) && $waiting['detail'] !== '' ? $waiting['detail'] : __('the Director is holding this job');

            return __('Held: :detail.', ['detail' => $detail]);
        }

        return '';
    }

    /**
     * Whether any turn is still going, so the page reads the thread again: `waiting` is worked out when read and
     * sends no change event.
     *
     * @param  list<array<string, mixed>>  $turns
     */
    public static function anyInProgress(array $turns): bool
    {
        foreach ($turns as $turn) {
            if (in_array($turn['state'] ?? null, ['queued', 'running', 'summarising'], true)) {
                return true;
            }
        }

        return false;
    }

    private static function ago(mixed $at): string
    {
        if (! is_string($at) || $at === '') {
            return '';
        }

        try {
            return CarbonImmutable::parse($at)->diffForHumans();
        } catch (Throwable) {
            return '';
        }
    }
}
