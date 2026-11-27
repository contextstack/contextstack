<?php

namespace App\Client;

/**
 * What the Director says this credential may do to one record: its `actions` booleans (`control`, `cancel`,
 * `decide`, `answer`, `adopt`, `archive`, as each kind applies). When a record carries `actions`, it alone decides
 * whether a control shows. A record without it (a Director before that field) falls back to the capability and
 * assignment rules the page used until then.
 */
final class RecordActions
{
    /**
     * @param  callable(): bool  $fallback  today's rule, used only when the record names no such action
     */
    public static function allows(mixed $record, string $action, callable $fallback): bool
    {
        $actions = is_array($record) ? ($record['actions'] ?? null) : null;
        if (is_array($actions) && array_key_exists($action, $actions)) {
            return $actions[$action] === true;
        }

        return $fallback();
    }

    /**
     * The same, with a capability as the fallback rule (the Blade `@allowed($record, 'cancel', 'control_work')`).
     */
    public static function allowsOr(mixed $record, string $action, string $capability): bool
    {
        return self::allows($record, $action, fn (): bool => app(Identity::class)->can($capability));
    }
}
