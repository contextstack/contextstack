<?php

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;

/**
 * Which conversations are hosted leads: the agent you talk to that starts task lanes for you. Every conversation
 * says so itself with `"lead": true` or `false` (CLIENT-API.md §11.1a).
 */
final class Leads
{
    /** Most leads fetched at once; a person keeps a handful. */
    public const FETCHED = 20;

    public function __construct(private readonly DirectorClient $director) {}

    /**
     * Lead conversation ids, the newest first.
     *
     * @return list<string>
     */
    public function conversationIds(): array
    {
        try {
            $answer = $this->director->get('conversations', ['lead' => '1', 'limit' => self::FETCHED]);
        } catch (DirectorRefused|DirectorUnavailable) {
            return [];
        }

        return self::among(array_values(array_filter((array) ($answer['conversations'] ?? []), 'is_array')));
    }

    /**
     * The leads among conversations already fetched, the newest first.
     *
     * @param  list<array<string, mixed>>  $conversations
     * @return list<string>
     */
    public static function among(array $conversations): array
    {
        $leads = array_values(array_filter($conversations, fn (array $conversation): bool => self::is($conversation)));
        usort($leads, fn (array $a, array $b): int => strcmp((string) ($b['created_at'] ?? ''), (string) ($a['created_at'] ?? '')));

        return array_values(array_unique(array_map(fn (array $conversation): string => $conversation['conversation_id'], $leads)));
    }

    /**
     * @param  array<string, mixed>  $conversation
     */
    public static function is(array $conversation): bool
    {
        return is_string($conversation['conversation_id'] ?? null) && ($conversation['lead'] ?? false) === true;
    }
}
