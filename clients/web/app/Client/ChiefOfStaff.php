<?php

declare(strict_types=1);

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;

/**
 * The chief of staff: one standing conversation that runs tasks, pinned in every client. The Director holds the slot
 * (`GET`/`PUT chief-of-staff`, owner only to change): turning it on starts its own lead conversation (or unarchives and
 * renames it), turning it off archives it, and while it is on the Director refuses to archive or delete that
 * conversation (409 `chief_of_staff`). This app keeps nothing about it.
 */
final class ChiefOfStaff
{
    /** Longest name the app offers for the slot: it labels the sidebar. */
    public const NAME_LIMIT = 40;

    /**
     * The slot. A Director that cannot be reached, or does not hold the slot, shows it off.
     *
     * @return array{enabled: bool, name: string, conversation_id: string|null}
     */
    public function slot(): array
    {
        // Read once per request: the layout, the sidebar and the page all ask.
        $cached = request()->attributes->get(self::class);
        if (is_array($cached)) {
            /** @var array{enabled: bool, name: string, conversation_id: string|null} $cached */
            return $cached;
        }

        try {
            $answer = app(DirectorClient::class)->find('chief-of-staff');
        } catch (DirectorRefused|DirectorUnavailable|NotConnected) {
            $answer = null;
        }
        $slot = self::read($answer);
        request()->attributes->set(self::class, $slot);

        return $slot;
    }

    /**
     * The chief of staff's conversation id while the slot is on, else null.
     */
    public function conversationId(): ?string
    {
        $slot = $this->slot();

        return $slot['enabled'] ? $slot['conversation_id'] : null;
    }

    /**
     * Turn the slot on or off and name it. Returns the conversation id when this turned it on, else null.
     *
     * @throws DirectorRefused|DirectorUnavailable|NotConnected when the Director cannot; the slot is left as it was
     */
    public function save(bool $enabled, string $name): ?string
    {
        $before = $this->slot();
        request()->attributes->remove(self::class);

        $slot = self::read(app(DirectorClient::class)->put('chief-of-staff', ['enabled' => $enabled, 'name' => $name], DirectorClient::newKey()));

        return $enabled && ! $before['enabled'] ? $slot['conversation_id'] : null;
    }

    /**
     * @param  array<string, mixed>|null  $answer
     * @return array{enabled: bool, name: string, conversation_id: string|null}
     */
    private static function read(?array $answer): array
    {
        $held = is_array($answer['chief_of_staff'] ?? null) ? $answer['chief_of_staff'] : [];

        return [
            'enabled' => ($held['enabled'] ?? false) === true,
            'name' => is_string($held['name'] ?? null) && $held['name'] !== '' ? $held['name'] : 'Chief of staff',
            'conversation_id' => is_string($held['conversation_id'] ?? null) && $held['conversation_id'] !== '' ? $held['conversation_id'] : null,
        ];
    }
}
