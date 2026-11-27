<?php

namespace App\Client;

/**
 * A hosted release approval (CLIENT-API.md "Hosted release approvals", docs/design/hosted-release-client.md): an
 * owner-only `install` or `rollback` action approval that a hosted lead asked for in its conversation. The node
 * inspects it (`pending`), pins the commit, target hash, schemas and release notes (`needs_owner`), and after the
 * owner approves it reports an outcome: the current release, the smoke result and a reason.
 */
final class ReleaseApproval
{
    public const array Kinds = ['install', 'rollback'];

    /** Outcome statuses that end a switch the node ran. */
    private const array Reported = ['installed', 'rolled_back', 'restored', 'rollback_failed', 'interrupted', 'timed_out', 'failed'];

    public static function is(mixed $approval): bool
    {
        return is_array($approval) && in_array($approval['kind'] ?? null, self::Kinds, true);
    }

    /**
     * Where the request is: `inspecting` (the node has not pinned it yet), `needs_owner`, `waiting` (approved; the
     * node has not reported), `reported` (an outcome came back), `denied` or `expired`.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function phase(array $approval): string
    {
        $status = $approval['status'] ?? null;
        $outcome = self::outcomeStatus($approval);

        return match (true) {
            $status === 'pending' => 'inspecting',
            $status === 'needs_owner' => 'needs_owner',
            $status === 'approved' && in_array($outcome, self::Reported, true) => 'reported',
            $status === 'approved' => 'waiting',
            $status === 'expired' => 'expired',
            default => 'denied',
        };
    }

    /**
     * @param  array<string, mixed>  $approval
     */
    public static function outcomeStatus(array $approval): ?string
    {
        $outcome = $approval['outcome'] ?? null;

        return is_array($outcome) && is_string($outcome['status'] ?? null) ? $outcome['status'] : null;
    }

    /**
     * A failed install whose automatic rollback still waits on running jobs, not yet acknowledged: it shows under
     * needs-you with the number of jobs left, and the owner may acknowledge it (intake stays frozen).
     *
     * @param  array<string, mixed>  $approval
     */
    public static function rollbackWaiting(array $approval): bool
    {
        $outcome = is_array($approval['outcome'] ?? null) ? $approval['outcome'] : [];

        return ($outcome['status'] ?? null) === 'rollback_failed' && is_int($outcome['waiting_jobs'] ?? null)
            && ($outcome['acknowledged'] ?? false) !== true;
    }

    /**
     * Whether this device may clear the request. The record's `actions.clear` alone decides when sent; otherwise
     * an owner may clear an open request, one approved and not reported, an interrupted one, or a rollback still
     * waiting on jobs, as the Director allows (director/src/lead_install.rs CLEARABLE).
     *
     * @param  array<string, mixed>  $approval
     */
    public static function clearable(array $approval): bool
    {
        return RecordActions::allows($approval, 'clear', function () use ($approval): bool {
            $outcome = is_array($approval['outcome'] ?? null) ? $approval['outcome'] : [];
            if (($outcome['cleared'] ?? false) === true || ($outcome['acknowledged'] ?? false) === true || ! self::ownerHere()) {
                return false;
            }

            return match ($approval['status'] ?? null) {
                'pending', 'needs_owner' => true,
                'approved' => in_array($outcome['status'] ?? null, [null, 'unknown', 'interrupted'], true) || self::rollbackWaiting($approval),
                default => false,
            };
        });
    }

    /**
     * Whether this device may approve or deny: the record's `actions.decide`, else the owner rule (release switches
     * are owner-only).
     *
     * @param  array<string, mixed>  $approval
     */
    public static function decidable(array $approval): bool
    {
        return ($approval['status'] ?? null) === 'needs_owner'
            && RecordActions::allows($approval, 'decide', fn (): bool => self::ownerHere());
    }

    /**
     * Whether this device may acknowledge a waiting rollback: `actions.answer`, else the owner rule.
     *
     * @param  array<string, mixed>  $approval
     */
    public static function acknowledgeable(array $approval): bool
    {
        return self::rollbackWaiting($approval) && RecordActions::allows($approval, 'answer', fn (): bool => self::ownerHere());
    }

    /**
     * The words and badge color for an outcome the node reported.
     *
     * @return array{0: string, 1: string}
     */
    public static function outcomeLabel(?string $status): array
    {
        return match ($status) {
            'installed' => [__('Installed; smoke passed'), 'green'],
            'rolled_back' => [__('Smoke failed; previous release restored'), 'amber'],
            'restored' => [__('Rolled back'), 'green'],
            'rollback_failed' => [__('Smoke failed; rollback not verified'), 'red'],
            'interrupted' => [__('Interrupted'), 'red'],
            'timed_out' => [__('Timed out without switching'), 'zinc'],
            'failed' => [__('Failed'), 'red'],
            default => [(string) $status, 'zinc'],
        };
    }

    /**
     * The approval a node-install wake turn reports on (its `detail.approval_id`, CLIENT-API.md), or null.
     *
     * @param  array<string, mixed>  $turn
     */
    public static function wakeApprovalId(array $turn): ?string
    {
        if (($turn['origin'] ?? null) !== 'wake' || ($turn['posted_by'] ?? null) !== 'node-install') {
            return null;
        }
        $detail = $turn['detail'] ?? null;
        $id = is_array($detail) ? ($detail['approval_id'] ?? null) : null;

        return is_string($id) && $id !== '' ? $id : null;
    }

    private static function ownerHere(): bool
    {
        $identity = app(Identity::class);

        return $identity->legacy() || (bool) ($identity->me()['owner'] ?? false);
    }
}
