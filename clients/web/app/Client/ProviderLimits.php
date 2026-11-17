<?php

declare(strict_types=1);

namespace App\Client;

use Carbon\CarbonImmutable;
use Throwable;

/**
 * Models a machine cannot run right now because their provider's usage limit was hit. Each node reports them in its
 * status (`status.provider_limits`: `{template, message, resets_at}`, one row per model); a row whose reset time has
 * passed no longer counts, even when the node has not reported since.
 */
final class ProviderLimits
{
    /**
     * Each machine's limited models, from a GET nodes answer: node name → model name → reset time (UTC ISO 8601, or
     * null when the provider did not say).
     *
     * @param  array<string, mixed>  $nodesAnswer
     * @return array<string, array<string, string|null>>
     */
    public static function fromNodes(array $nodesAnswer, ?CarbonImmutable $now = null): array
    {
        $limits = [];
        foreach ((array) ($nodesAnswer['nodes'] ?? []) as $node) {
            if (! is_array($node) || ! is_string($node['name'] ?? null)) {
                continue;
            }
            $rows = self::ofNode($node, $now);
            if ($rows !== []) {
                $limits[$node['name']] = $rows;
            }
        }

        return $limits;
    }

    /**
     * One node record's limited models: model name → reset time or null.
     *
     * @param  array<string, mixed>  $node
     * @return array<string, string|null>
     */
    public static function ofNode(array $node, ?CarbonImmutable $now = null): array
    {
        $now ??= CarbonImmutable::now();
        $status = is_array($node['status'] ?? null) ? $node['status'] : [];
        $limited = [];
        foreach ((array) ($status['provider_limits'] ?? []) as $row) {
            if (! is_array($row) || ! is_string($row['template'] ?? null) || $row['template'] === '') {
                continue;
            }
            $resets = is_string($row['resets_at'] ?? null) && $row['resets_at'] !== '' ? $row['resets_at'] : null;
            $at = $resets === null ? null : self::parse($resets);
            if ($at !== null && $at->lessThanOrEqualTo($now)) {
                continue;
            }
            $limited[$row['template']] = $resets;
        }
        ksort($limited);

        return $limited;
    }

    /**
     * "out of quota until Nov 17", or "out of quota" when the reset time is unknown.
     */
    public static function phrase(?string $resetsAt): string
    {
        $at = $resetsAt === null ? null : self::parse($resetsAt);

        return $at === null
            ? __('out of quota')
            : __('out of quota until :date', ['date' => $at->setTimezone(config('app.timezone'))->format('M j')]);
    }

    /**
     * The model to preselect: $current, unless that machine is out of quota for it and one of $candidates (ready
     * models, in the picker's order) is not.
     *
     * @param  list<string>  $candidates
     * @param  array<string, string|null>  $limited  model name → reset time, for the machine the model would run on
     */
    public static function prefer(string $current, array $candidates, array $limited): string
    {
        if (! array_key_exists($current, $limited)) {
            return $current;
        }
        foreach ($candidates as $candidate) {
            if (! array_key_exists($candidate, $limited)) {
                return $candidate;
            }
        }

        return $current;
    }

    private static function parse(string $time): ?CarbonImmutable
    {
        try {
            return CarbonImmutable::parse($time);
        } catch (Throwable) {
            return null;
        }
    }
}
