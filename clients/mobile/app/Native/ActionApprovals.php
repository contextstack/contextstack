<?php

namespace App\Native;

use App\Client\Json;

/**
 * Words and formatting for action approvals (CLIENT-API.md "Action approvals"), shared by the screens that show them.
 */
final class ActionApprovals
{
    /**
     * The command as one shell-quoted line, so what is shown is exactly what would run.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function command(array $approval): string
    {
        return implode(' ', array_map(static function (string $part): string {
            return $part !== '' && preg_match('/^[A-Za-z0-9_@%+=:,.\/-]+$/', $part) === 1
                ? $part
                : "'".str_replace("'", "'\\''", $part)."'";
        }, Json::strings($approval['argv'] ?? null)));
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
