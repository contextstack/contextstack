<?php

namespace App\Native;

use App\Client\Json;

/**
 * Words for hosted release approvals (CLIENT-API.md "Hosted release approvals", docs/design/hosted-release-client.md):
 * an owner-only `install` of a staged bundle or the node's local main, or a manual `rollback` to the previous release.
 * The node inspects the request (`pending`), pins it (`needs_owner`: commit, target hash, both schema versions and the
 * release notes diff), and after approval reports a terminal `outcome` with the current release and the smoke result.
 */
final class ReleaseApprovals
{
    /**
     * @param  array<string, mixed>  $approval
     */
    public static function isRelease(array $approval): bool
    {
        return in_array($approval['kind'] ?? null, ['install', 'rollback'], true);
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function headline(array $approval): string
    {
        if (($approval['kind'] ?? null) === 'rollback') {
            return 'Roll back to the previous release';
        }

        return 'Install '.self::source($approval);
    }

    /**
     * What the install asks for: the named staged bundle, or the node's local main (no bundle named).
     *
     * @param  array<string, mixed>  $approval
     */
    public static function source(array $approval): string
    {
        $bundle = Json::string($approval['bundle'] ?? null);

        return $bundle !== '' ? 'bundle '.$bundle : 'local main';
    }

    /**
     * Where the request stands, before any outcome words.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function status(array $approval): string
    {
        return match ($approval['status'] ?? null) {
            'pending' => 'The node is inspecting this release',
            'needs_owner' => 'Pinned; needs an owner',
            'approved' => 'Approved',
            'denied' => 'Denied',
            'expired' => 'Expired',
            default => Json::string($approval['status'] ?? null, 'Unknown'),
        };
    }

    /**
     * A schema version, or "No local Director" when the machine serves none (the field is absent).
     */
    public static function schema(mixed $version): string
    {
        return is_int($version) ? (string) $version : 'No local Director';
    }

    /**
     * Whether the node has pinned the release: the full commit and target hash are known.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function pinned(array $approval): bool
    {
        return Json::string($approval['commit'] ?? null) !== '' && Json::string($approval['target_hash'] ?? null) !== '';
    }

    /**
     * @param  array<string, mixed>  $approval
     * @return array<string, mixed>
     */
    public static function outcome(array $approval): array
    {
        return Json::record($approval['outcome'] ?? null);
    }

    /**
     * Approved and the node has not reported yet: the switch and the smoke trial are under way (or not picked up).
     *
     * @param  array<string, mixed>  $approval
     */
    public static function waiting(array $approval): bool
    {
        $status = self::outcome($approval)['status'] ?? null;

        return ($approval['status'] ?? null) === 'approved' && ($status === null || $status === 'unknown');
    }

    /**
     * What happened, in words.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function outcomeWords(array $approval): string
    {
        $outcome = self::outcome($approval);
        if (self::waiting($approval)) {
            return ($approval['claimed_at'] ?? null) !== null
                ? 'Approved. The node is switching releases and running the smoke trial; this updates when it reports.'
                : 'Approved. Waiting for the node to pick it up.';
        }
        $jobs = $outcome['waiting_jobs'] ?? null;

        return match ($outcome['status'] ?? null) {
            'waiting_on_owner' => 'Waiting on an owner. Nothing has switched.',
            'installed' => 'Installed. The smoke trial passed.',
            'rolled_back' => 'The smoke trial failed, so the previous release was restored.',
            'restored' => 'Rolled back to the previous release.',
            'rollback_failed' => 'The smoke trial failed and the rollback has not finished: the failed release is live'
                .(is_int($jobs) ? ', waiting on '.$jobs.' '.($jobs === 1 ? 'job' : 'jobs').'.' : '.'),
            'interrupted' => ($outcome['cleared'] ?? false) === true ? 'Cleared by an owner.' : 'Interrupted.',
            'timed_out' => 'Timed out without switching; the current release stays.',
            'failed' => 'The node could not complete the switch.',
            'unavailable' => 'The node could not inspect and pin this release. Request it again after checking the node.',
            'expired' => 'Expired before it was decided.',
            'denied' => 'Denied. Nothing switched.',
            default => ($approval['status'] ?? null) === 'denied' ? 'Denied. Nothing switched.' : '',
        };
    }

    /**
     * The smoke result of an outcome (a request's own, or `last_smoke`): '' when it carries none.
     *
     * @param  array<string, mixed>  $outcome
     */
    public static function smoke(array $outcome): string
    {
        return match ($outcome['smoke_ok'] ?? null) {
            true => 'Smoke passed',
            false => 'Smoke failed',
            default => '',
        };
    }

    /**
     * The smoke checks the node reported, each as one line. `name` and `ok` are required (CLIENT-API.md); a check
     * missing either is malformed and skipped.
     *
     * @param  array<string, mixed>  $outcome
     * @return list<string>
     */
    public static function checks(array $outcome): array
    {
        $lines = [];
        foreach (Json::records(Json::record($outcome['smoke'] ?? null)['checks'] ?? null) as $check) {
            $name = Json::string($check['name'] ?? null);
            $ok = $check['ok'] ?? null;
            if ($name === '' || ! is_bool($ok)) {
                continue;
            }
            $mark = $ok ? 'passed' : 'failed';
            $reason = Json::string($check['reason'] ?? null);
            $lines[] = $name.': '.$mark.($reason !== '' ? ' ('.$reason.')' : '');
        }

        return $lines;
    }

    /**
     * Whether an owner may still clear it (mirrors the Director's rule): open, approved without a final report,
     * interrupted, or a failed rollback still waiting on jobs.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function clearable(array $approval): bool
    {
        $status = $approval['status'] ?? null;
        $outcome = self::outcome($approval);
        if (in_array($status, ['pending', 'needs_owner'], true)) {
            return true;
        }
        if ($status !== 'approved') {
            return false;
        }

        return match ($outcome['status'] ?? null) {
            null, 'unknown' => true,
            'interrupted' => ($outcome['cleared'] ?? false) !== true,
            'rollback_failed' => is_int($outcome['waiting_jobs'] ?? null),
            default => false,
        };
    }

    /**
     * A failed rollback still waiting on jobs that no one has acknowledged yet.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function acknowledgeable(array $approval): bool
    {
        $outcome = self::outcome($approval);

        return ($outcome['status'] ?? null) === 'rollback_failed' && is_int($outcome['waiting_jobs'] ?? null)
            && ($outcome['acknowledged'] ?? false) !== true;
    }
}
