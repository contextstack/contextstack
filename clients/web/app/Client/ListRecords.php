<?php

namespace App\Client;

use Illuminate\Support\Str;

/**
 * The fields a list page shows for an intent or a job. List pages hold every recent record in Livewire state, which
 * travels with each request, so they keep only these instead of the Director's full records (whose terminal rows,
 * file lists, budgets and checksums passed Livewire's 1MB request limit at a few hundred records).
 */
final class ListRecords
{
    /** Longest task text kept when an intent has no title; the list shows it as the link text. */
    public const TASK_LIMIT = 200;

    /**
     * @param  array<string, mixed>  $intent
     * @return array<string, mixed>
     */
    public static function intent(array $intent): array
    {
        $meta = is_array($intent['meta'] ?? null) ? $intent['meta'] : [];

        return [
            'intent_id' => $intent['intent_id'] ?? null,
            'revision' => $intent['revision'] ?? null,
            'created_at' => $intent['created_at'] ?? null,
            'node' => $intent['node'] ?? null,
            'title' => $intent['title'] ?? null,
            'task' => is_string($intent['task'] ?? null) ? Str::limit($intent['task'], self::TASK_LIMIT) : null,
            'meta' => array_intersect_key($meta, array_flip(['source', 'repo', 'lane'])),
        ];
    }

    /**
     * @param  array<string, mixed>  $job
     * @return array<string, mixed>
     */
    public static function job(array $job): array
    {
        $cancel = is_array($job['cancel'] ?? null) ? $job['cancel'] : null;
        $land = is_array($job['land'] ?? null) ? $job['land'] : null;

        return [
            'job_id' => $job['job_id'] ?? null,
            'intent_id' => $job['intent_id'] ?? null,
            'revision' => $job['revision'] ?? null,
            'created_at' => $job['created_at'] ?? null,
            'state' => $job['state'] ?? null,
            'cancel' => $cancel === null ? null : ['state' => $cancel['state'] ?? null],
            'land' => $land === null ? null : [
                'state' => $land['state'] ?? null,
                'commit' => $land['commit'] ?? null,
                'reason' => is_string($land['reason'] ?? null) ? Str::limit($land['reason'], self::TASK_LIMIT) : null,
            ],
        ];
    }

    /**
     * @param  array<string, mixed>  $record
     * @return array<string, mixed>
     */
    public static function of(string $kind, array $record): array
    {
        return match ($kind) {
            'intent' => self::intent($record),
            'job' => self::job($record),
            'conversation' => self::conversation($record),
            default => $record,
        };
    }

    /**
     * The fields the conversation list shows, with a "needs you" question cut to a line's worth.
     *
     * @param  array<string, mixed>  $conversation
     * @return array<string, mixed>
     */
    public static function conversation(array $conversation): array
    {
        $attention = is_array($conversation['attention'] ?? null) ? $conversation['attention'] : null;
        $budget = is_array($conversation['budget'] ?? null) ? $conversation['budget'] : null;

        return [
            ...array_intersect_key($conversation, array_flip(['conversation_id', 'revision', 'title', 'model', 'node', 'state', 'turns', 'created_at', 'lead', 'archived_at'])),
            'queued_turn_ids' => array_values(array_filter((array) ($conversation['queued_turn_ids'] ?? []), 'is_string')),
            'budget' => $budget === null ? null : ['shown' => $budget['shown'] ?? null],
            'attention' => $attention === null ? null : [
                'question' => is_string($attention['question'] ?? null) ? Str::limit($attention['question'], self::TASK_LIMIT) : null,
                'source_kind' => $attention['source_kind'] ?? null,
                'set_at' => $attention['set_at'] ?? null,
            ],
        ];
    }

    /**
     * One entry per task lane: a retry is another job on the same intent (cs `retry_task`), so a lane's jobs are
     * its attempts. Each entry carries the newest attempt as `job` and every attempt, oldest first, as `attempts`.
     * Entries come newest lane first, by when the lane's newest attempt was made. A job without an intent is a lane
     * of its own.
     *
     * @param  list<array<string, mixed>>  $jobs
     * @return list<array{job: array<string, mixed>, attempts: list<array<string, mixed>>}>
     */
    public static function lanes(array $jobs): array
    {
        usort($jobs, fn (array $a, array $b): int => strcmp((string) ($a['created_at'] ?? ''), (string) ($b['created_at'] ?? ''))
            ?: ((int) ($a['revision'] ?? 0) <=> (int) ($b['revision'] ?? 0)));

        $lanes = [];
        foreach ($jobs as $job) {
            $key = is_string($job['intent_id'] ?? null) ? 'intent:'.$job['intent_id'] : 'job:'.($job['job_id'] ?? '');
            $lanes[$key][] = $job;
        }

        $entries = array_map(fn (array $attempts): array => ['job' => $attempts[array_key_last($attempts)], 'attempts' => $attempts], array_values($lanes));
        usort($entries, fn (array $a, array $b): int => strcmp((string) ($b['job']['created_at'] ?? ''), (string) ($a['job']['created_at'] ?? '')));

        return $entries;
    }
}
