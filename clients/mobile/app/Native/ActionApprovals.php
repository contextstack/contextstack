<?php

namespace App\Native;

use App\Client\Json;

/**
 * Words and formatting for action approvals (CLIENT-API.md "Action approvals"), shared by the screens that show them.
 */
final class ActionApprovals
{
    /**
     * The command as one shell-quoted line. Characters a person cannot see or that reorder text (control characters,
     * bidi overrides, zero-width marks) are shown as visible \u{…} escapes, so the line cannot read differently from
     * what would run; `hidden()` says when that happened, and `malformed()` when an argv element is not a string.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function command(array $approval): string
    {
        $argv = is_array($approval['argv'] ?? null) ? $approval['argv'] : [];

        return implode(' ', array_map(static function (mixed $part): string {
            if (! is_string($part)) {
                return '<not a string: '.get_debug_type($part).'>';
            }
            $part = (string) preg_replace_callback('/\p{C}/u', fn (array $m): string => sprintf('\\u{%04X}', mb_ord($m[0])), $part);

            return $part !== '' && preg_match('/^[A-Za-z0-9_@%+=:,.\/-]+$/', $part) === 1
                ? $part
                : "'".str_replace("'", "'\\''", $part)."'";
        }, array_values($argv)));
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function hidden(array $approval): bool
    {
        foreach (is_array($approval['argv'] ?? null) ? $approval['argv'] : [] as $part) {
            if (is_string($part) && preg_match('/\p{C}/u', $part) === 1) {
                return true;
            }
        }

        return false;
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function malformed(array $approval): bool
    {
        $argv = $approval['argv'] ?? null;

        return ! is_array($argv) || $argv === [] || array_filter($argv, fn (mixed $part): bool => ! is_string($part)) !== [];
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function status(array $approval): string
    {
        return match ($approval['status'] ?? null) {
            'pending' => 'Under review',
            'needs_owner' => 'Needs you',
            'denied' => 'Denied',
            'expired' => 'Expired: its job ended before it was decided',
            'approved' => ($approval['decided_by'] ?? null) !== null ? 'Approved by you' : 'Approved by the reviewer',
            default => Json::string($approval['status'] ?? null, 'Unknown'),
        };
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function verdict(array $approval): string
    {
        if (! is_string($approval['reviewer_model'] ?? null) || $approval['reviewer_model'] === '') {
            return 'No reviewer model was selected or called.';
        }

        $verdict = match ($approval['verdict'] ?? null) {
            'go' => 'Go',
            'no_go' => 'No go',
            default => 'No verdict',
        };

        return $verdict.' from '.$approval['reviewer_model'];
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function outcome(array $approval): string
    {
        $outcome = Json::record($approval['outcome'] ?? null);

        return match ($outcome['status'] ?? null) {
            'waiting_on_owner' => 'Waiting on you. Nothing has run.',
            'denied' => 'Did not run.',
            'succeeded' => 'Ran and succeeded.',
            'failed' => 'Ran and failed'.(is_int($outcome['exit_code'] ?? null) ? ' (exit '.$outcome['exit_code'].').' : '.'),
            default => ($approval['claimed_at'] ?? null) !== null ? 'Claimed by the run; it may have run.' : 'Not run yet.',
        };
    }
}
