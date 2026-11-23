<?php

namespace App\Native;

use App\Client\Json;

/**
 * Words for backlog items (CLIENT-API.md "Project backlog and heartbeat"), shared by the screens that show them.
 */
final class Backlog
{
    public const array Open = ['next', 'now', 'blocked'];

    /**
     * @param  array<string, mixed>  $item
     */
    public static function status(array $item): string
    {
        return match (Json::string($item['status'] ?? null, 'next')) {
            'now' => 'Now',
            'blocked' => 'Blocked',
            'done' => 'Done',
            'dropped' => 'Dropped',
            default => 'Next',
        };
    }

    /**
     * @param  array<string, mixed>  $item
     */
    public static function waitsOnOwner(array $item): bool
    {
        foreach (Json::records($item['blocked_on'] ?? null) as $blocker) {
            if (($blocker['kind'] ?? null) === 'owner') {
                return true;
            }
        }

        return false;
    }

    /**
     * What blocks the item, in words: "you", "external: Apple review", "item bli_2", "job job_9".
     *
     * @param  array<string, mixed>  $item
     */
    public static function blockers(array $item): string
    {
        $words = [];
        foreach (Json::records($item['blocked_on'] ?? null) as $blocker) {
            $kind = Json::string($blocker['kind'] ?? null);
            $ref = Json::string($blocker['ref'] ?? null);
            $words[] = $kind === 'owner' ? 'you' : trim($kind.' '.$ref);
        }

        return implode(', ', $words);
    }

    /**
     * Items in the Director's order: priority, then item id.
     *
     * @param  list<array<string, mixed>>  $items
     * @return list<array<string, mixed>>
     */
    public static function ordered(array $items): array
    {
        usort($items, fn (array $a, array $b): int => [self::priority($a), Json::string($a['item_id'] ?? null)] <=> [self::priority($b), Json::string($b['item_id'] ?? null)]);

        return $items;
    }

    /**
     * @param  array<string, mixed>  $item
     */
    private static function priority(array $item): int
    {
        return is_int($item['priority'] ?? null) ? $item['priority'] : 0;
    }
}
