<?php

namespace App\Client;

/**
 * Typed reads of the Director's JSON answers. The Client API documents each field's type; these helpers turn a
 * missing or wrongly typed field into a plain default instead of letting `mixed` flow into the screens.
 */
final class Json
{
    public static function string(mixed $value, string $default = ''): string
    {
        return match (true) {
            is_string($value) => $value,
            is_int($value), is_float($value) => (string) $value,
            default => $default,
        };
    }

    public static function nullableString(mixed $value): ?string
    {
        return is_string($value) && $value !== '' ? $value : null;
    }

    /**
     * A JSON object, as an array keyed by its field names.
     *
     * @return array<string, mixed>
     */
    public static function record(mixed $value): array
    {
        if (! is_array($value)) {
            return [];
        }

        $record = [];
        foreach ($value as $key => $field) {
            $record[(string) $key] = $field;
        }

        return $record;
    }

    /**
     * A JSON array of objects; anything that is not an object is skipped.
     *
     * @return list<array<string, mixed>>
     */
    public static function records(mixed $value): array
    {
        if (! is_array($value)) {
            return [];
        }

        return array_values(array_map(self::record(...), array_filter($value, is_array(...))));
    }

    /**
     * A JSON array of strings (and numbers, as text); anything else is skipped.
     *
     * @return list<string>
     */
    public static function strings(mixed $value): array
    {
        if (! is_array($value)) {
            return [];
        }

        return array_values(array_map(self::string(...), array_filter($value, fn (mixed $item): bool => is_string($item) || is_int($item) || is_float($item))));
    }
}
