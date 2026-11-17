<?php

namespace App\Client;

/**
 * The client's half of live updates (CLIENT-API.md §8, §8.1). The browser keeps the stream open and follows the
 * reconnect order (open, wait for hello, hold, refetch, then apply the held and live events); these are the two
 * rules a page applies them by.
 */
final class LiveUpdates
{
    /**
     * Apply one change event to the records a page holds, keyed "kind:id". An event is applied only when its
     * revision is higher than that of the record held with the same kind and id; a record not held yet is applied.
     *
     * @param  array<string, array<string, mixed>>  $records
     * @param  array<string, mixed>  $change  {kind, id, revision, record}
     * @return array<string, array<string, mixed>>|null the new records, or null when the event changes nothing
     */
    public static function applyChange(array $records, array $change): ?array
    {
        $kind = $change['kind'] ?? null;
        $id = $change['id'] ?? null;
        $revision = $change['revision'] ?? null;
        $record = $change['record'] ?? null;

        if (! is_string($kind) || ! is_scalar($id) || ! is_int($revision) || ! is_array($record)) {
            return null;
        }

        $key = $kind.':'.$id;
        $held = $records[$key]['revision'] ?? null;

        if (is_int($held) && $revision <= $held) {
            return null;
        }

        $records[$key] = $record;

        return $records;
    }

    /**
     * Append a run's live events to the output shown for it: only events whose seq is above the last one shown,
     * in seq order. `delta` events carry text; other kinds (`stop`) only move the seq on.
     *
     * @param  array{seq: int, text: string}  $output
     * @param  list<array<string, mixed>>  $events
     * @return array{seq: int, text: string}
     */
    public static function appendOutput(array $output, array $events): array
    {
        usort($events, fn (array $a, array $b): int => ((int) ($a['seq'] ?? 0)) <=> ((int) ($b['seq'] ?? 0)));

        foreach ($events as $event) {
            $seq = $event['seq'] ?? null;
            if (! is_int($seq) || $seq <= $output['seq']) {
                continue;
            }
            $text = $event['payload']['text'] ?? null;
            if (($event['kind'] ?? null) === 'delta' && is_string($text)) {
                $output['text'] .= $text;
            }
            $output['seq'] = $seq;
        }

        return $output;
    }

    /** Most live text a lane or task page keeps for a run: the end of it, as a terminal would show. */
    public const OUTPUT_TAIL = 60000;

    /** Most pages of 1000 events fetched when catching up one run in one request. */
    private const CATCH_UP_PAGES = 20;

    /**
     * Fetch a run's events after what is shown and append them (GET runs/{id}/events?after=SEQ, 1000 at most each).
     * Null when nothing was shown yet and the first fetch failed: the caller holds nothing for the run, so the next
     * event tries again instead of starting after a gap.
     *
     * @param  array{seq: int, text: string}|null  $output
     * @param  callable(int): mixed  $fetch  the events after a seq, as the Director's answer, or null on failure
     * @return array{seq: int, text: string}|null
     */
    public static function catchUp(?array $output, callable $fetch, ?int $tail = null): ?array
    {
        $caught = $output ?? ['seq' => 0, 'text' => ''];
        $fetched = false;
        $pages = 0;
        do {
            $answer = $fetch($caught['seq']);
            if (! is_array($answer)) {
                break;
            }
            $fetched = true;
            $events = array_values(array_filter((array) ($answer['events'] ?? []), 'is_array'));
            $before = $caught['seq'];
            $caught = self::appendOutput($caught, $events);
        } while (count($events) >= 1000 && $caught['seq'] > $before && ++$pages < self::CATCH_UP_PAGES);

        if (! $fetched && $output === null) {
            return null;
        }

        return $tail === null ? $caught : self::tail($caught, $tail);
    }

    /**
     * The output with only the end of its text kept, marked with an ellipsis where the start was cut.
     *
     * @param  array{seq: int, text: string}  $output
     * @return array{seq: int, text: string}
     */
    public static function tail(array $output, int $limit = self::OUTPUT_TAIL): array
    {
        if (strlen($output['text']) > $limit) {
            $output['text'] = '…'.mb_strcut($output['text'], strlen($output['text']) - $limit);
        }

        return $output;
    }
}
