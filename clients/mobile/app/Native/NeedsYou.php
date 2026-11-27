<?php

namespace App\Native;

use App\Client\Json;

/**
 * The choices a needs-you item offers (docs/design/hosted-needs-you-client.md): conversation attention carries them at
 * its top level, a backlog item's owner blocker under `needs_you`. Each option is a button labelled with its label;
 * the default is highlighted, and nothing is sent until a button is pressed.
 */
final class NeedsYou
{
    /** The in-app screens a link can open; `run` and `lane` have no screen of their own on the device yet. */
    public const array Openable = ['approval', 'job', 'backlog'];

    /**
     * @param  array<string, mixed>  $source
     * @return list<array{id: string, label: string}>
     */
    public static function options(array $source): array
    {
        $options = [];
        foreach (Json::records($source['options'] ?? null) as $option) {
            $id = Json::string($option['id'] ?? null);
            if ($id !== '') {
                $options[] = ['id' => $id, 'label' => Json::string($option['label'] ?? null, $id)];
            }
        }

        return $options;
    }

    /**
     * @param  array<string, mixed>  $source
     */
    public static function default(array $source): string
    {
        return Json::string($source['default'] ?? null);
    }

    /**
     * The links the item carries that open a screen here, kind => record ID.
     *
     * @param  array<string, mixed>  $source
     * @return array<string, string>
     */
    public static function links(array $source): array
    {
        $links = [];
        // Attention that links an approval without a question names it here (CLIENT-API.md "Action approvals").
        $approval = Json::string($source['action_approval_id'] ?? null);
        if ($approval !== '') {
            $links['approval'] = $approval;
        }
        foreach (Json::record($source['links'] ?? null) as $kind => $id) {
            $id = Json::string($id);
            if ($id !== '' && in_array($kind, self::Openable, true)) {
                $links[$kind] = $id;
            }
        }

        return $links;
    }

    public static function linkWords(string $kind): string
    {
        return match ($kind) {
            'approval' => 'Open the approval',
            'job' => 'Open the job',
            'backlog' => 'Open the backlog item',
            default => 'Open',
        };
    }

    /**
     * The owner blocker's needs-you choices on a backlog item, or [] when it has none configured.
     *
     * @param  array<string, mixed>  $item
     * @return array<string, mixed>
     */
    public static function ofBacklogItem(array $item): array
    {
        foreach (Json::records($item['blocked_on'] ?? null) as $blocker) {
            if (($blocker['kind'] ?? null) === 'owner' && is_array($blocker['needs_you'] ?? null)) {
                return Json::record($blocker['needs_you']);
            }
        }

        return [];
    }
}
