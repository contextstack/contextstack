<?php

namespace App\Client;

/**
 * The order conversations are listed in: those waiting on your answer first, then your leads, then the newest.
 */
final class ConversationOrder
{
    /**
     * @param  list<array<string, mixed>>  $conversations
     * @param  list<string>  $leadIds
     * @return list<array<string, mixed>>
     */
    public static function sort(array $conversations, array $leadIds): array
    {
        usort($conversations, function (array $a, array $b) use ($leadIds): int {
            $aNeeds = self::needsYou($a);
            $bNeeds = self::needsYou($b);
            if ($aNeeds !== $bNeeds) {
                return $bNeeds <=> $aNeeds;
            }
            $aLead = in_array($a['conversation_id'] ?? null, $leadIds, true);
            $bLead = in_array($b['conversation_id'] ?? null, $leadIds, true);
            if ($aLead !== $bLead) {
                return $bLead <=> $aLead;
            }

            return strcmp((string) ($b['created_at'] ?? ''), (string) ($a['created_at'] ?? '')) ?: ((int) ($b['revision'] ?? 0) <=> (int) ($a['revision'] ?? 0));
        });

        return $conversations;
    }

    /**
     * @param  array<string, mixed>  $conversation
     */
    public static function needsYou(array $conversation): bool
    {
        return is_array($conversation['attention'] ?? null) && filled($conversation['attention']['question'] ?? null);
    }

    /**
     * @param  array<string, mixed>  $conversation
     */
    public static function name(array $conversation): string
    {
        return filled($conversation['title'] ?? null)
            ? (string) $conversation['title']
            : __('Untitled conversation with :model', ['model' => $conversation['model'] ?? __('an agent')]);
    }
}
