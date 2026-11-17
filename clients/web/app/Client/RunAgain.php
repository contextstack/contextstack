<?php

namespace App\Client;

/**
 * "Run again" for a job (CLIENT-API.md §4.1): `from_job`, which offers the job's bundle again. For a bundle made before
 * there was a "no check" check (so its runs always ended "fail"), the Director adds that check itself and records
 * `meta.check_added: "none"` on the new job.
 */
final class RunAgain
{
    /**
     * The POST intents body.
     *
     * @param  array<string, mixed>  $intent  the intent the job carried out
     * @param  array<string, mixed>  $job
     * @return array<string, mixed>
     */
    public static function body(array $intent, array $job): array
    {
        return array_filter([
            'node' => $intent['node'], 'spec' => $intent['spec'], 'task' => $intent['task'], 'from_job' => (string) $job['job_id'],
            'trials' => $intent['trials'] ?? 1, 'grant' => $intent['grant'] ?? null, 'meta' => ($intent['meta'] ?? []) ?: null,
            'title' => $intent['title'] ?? null, 'project_id' => $intent['project_id'] ?? null,
        ], fn ($value) => $value !== null);
    }

    /**
     * Whether the Director added the "no check" check to the new job.
     *
     * @param  array<string, mixed>  $answer  the POST intents answer
     */
    public static function addedCheck(array $answer): bool
    {
        foreach ((array) ($answer['jobs'] ?? []) as $job) {
            if (is_array($job) && ($job['meta']['check_added'] ?? null) === 'none') {
                return true;
            }
        }

        return false;
    }
}
