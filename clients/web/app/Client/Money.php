<?php

namespace App\Client;

/**
 * Dollars as text a person reads, by the Director's rule (CLIENT-API.md §12.2): whole cents show as cents; anything
 * finer shows more digits (under $1, three significant figures; from $1, four decimals), cut and never rounded up,
 * so a little spent never looks like nothing spent.
 */
final class Money
{
    public static function dollars(mixed $value): string
    {
        if (! is_numeric($value)) {
            return '—';
        }
        $micros = max(0, (int) round((float) $value * 1_000_000));
        $whole = intdiv($micros, 1_000_000);
        $fraction = $micros % 1_000_000;
        if ($fraction % 10_000 === 0) {
            $places = 2;
        } elseif ($whole > 0) {
            $places = 4;
        } else {
            // Leading zeros after the point, then three significant figures.
            $places = min(6, 6 - strlen((string) $fraction) + 3);
        }
        $digits = rtrim(substr(sprintf('%06d', $fraction), 0, $places), '0');

        return '$'.$whole.'.'.str_pad($digits, 2, '0');
    }
}
