<?php

declare(strict_types=1);

namespace App\Client;

/**
 * Director GET models: choices for the new-conversation picker and the failed-turn model switch.
 */
final class ModelCatalog
{
    public const string NoReadyHint = 'Sign in to a subscription CLI (for example `claude`) or add an API key (`cs init --api-key openai`), then reload this page.';

    /**
     * @param  array<string, mixed>  $catalog
     * @return array{
     *     choices: list<array{
     *         name: string,
     *         label: string,
     *         available: bool,
     *         display_name: string,
     *         provider: string,
     *         billing: string,
     *         reason: string|null,
     *         lead: bool,
     *     }>,
     *     default: string,
     *     canStart: bool,
     * }
     */
    public static function parse(array $catalog): array
    {
        $choices = [];
        foreach ((array) ($catalog['models'] ?? []) as $row) {
            if (! is_array($row) || ! is_string($row['name'] ?? null)) {
                continue;
            }
            $name = $row['name'];
            if (in_array($name, array_column($choices, 'name'), true)) {
                continue; // the Director may list a model more than once; the picker shows it once
            }
            $costClass = is_string($row['cost_class'] ?? null) ? $row['cost_class'] : '';
            $billing = is_string($row['billing'] ?? null) ? $row['billing'] : self::billingFromCostClass($costClass);
            $displayName = is_string($row['display_name'] ?? null)
                ? $row['display_name']
                : self::displayNameFromLabel((string) ($row['label'] ?? $name));
            $provider = is_string($row['provider'] ?? null) && $row['provider'] !== ''
                ? $row['provider']
                : 'local';
            $available = (bool) ($row['available'] ?? true);
            $reason = is_string($row['unavailable_reason'] ?? null) ? $row['unavailable_reason'] : null;
            // Whether a hosted lead can run on it: the catalog always says.
            $lead = ($row['lead'] ?? false) === true;

            $choices[] = [
                'name' => $name,
                'label' => self::optionLabel($displayName, $provider, $billing, $available, $reason),
                'available' => $available,
                'display_name' => $displayName,
                'provider' => $provider,
                'billing' => $billing,
                'reason' => $reason,
                'lead' => $lead,
            ];
        }

        $defaultFromDirector = is_string($catalog['default_model'] ?? null) ? $catalog['default_model'] : '';
        $default = self::resolveDefault($choices, $defaultFromDirector);
        $canStart = self::hasReady($choices);

        return [
            'choices' => $choices,
            'default' => $default,
            'canStart' => $canStart,
        ];
    }

    /**
     * Ready models only, for switching a failed turn to another model.
     *
     * @return list<array{name: string, label: string, available: bool, display_name: string}>
     */
    public static function availableChoices(DirectorClient $director): array
    {
        $choices = [];
        foreach (self::parse($director->get('models'))['choices'] as $choice) {
            if ($choice['available']) {
                $choices[] = ['name' => $choice['name'], 'label' => $choice['label'], 'available' => true, 'display_name' => $choice['display_name']];
            }
        }

        return $choices;
    }

    /**
     * The plain model name for a sentence ("Continued … on Codex").
     *
     * @param  list<array{name: string, label: string, available: bool, display_name: string}>  $choices
     */
    public static function labelFor(string $name, array $choices): string
    {
        foreach ($choices as $choice) {
            if ($choice['name'] === $name) {
                return $choice['display_name'];
            }
        }

        return $name;
    }

    /**
     * @param  list<array{available: bool, billing: string, name: string}>  $choices
     */
    private static function resolveDefault(array $choices, string $defaultFromDirector): string
    {
        if ($defaultFromDirector !== '') {
            foreach ($choices as $choice) {
                if ($choice['name'] === $defaultFromDirector && $choice['available']) {
                    return $defaultFromDirector;
                }
            }
        }

        foreach ($choices as $choice) {
            if ($choice['available'] && $choice['billing'] === 'subscription') {
                return $choice['name'];
            }
        }

        foreach ($choices as $choice) {
            if ($choice['available']) {
                return $choice['name'];
            }
        }

        return '';
    }

    /**
     * @param  list<array{available: bool}>  $choices
     */
    private static function hasReady(array $choices): bool
    {
        foreach ($choices as $choice) {
            if ($choice['available']) {
                return true;
            }
        }

        return false;
    }

    private static function billingFromCostClass(string $costClass): string
    {
        return match ($costClass) {
            'free_local' => 'free_local',
            'api', 'payg', 'pay_as_you_go' => 'paid_api',
            default => 'subscription',
        };
    }

    private static function displayNameFromLabel(string $label): string
    {
        $paren = strpos($label, ' (');
        if ($paren === false) {
            return $label;
        }

        return substr($label, 0, $paren);
    }

    private static function optionLabel(
        string $displayName,
        string $provider,
        string $billing,
        bool $available,
        ?string $reason,
    ): string {
        $billingLabel = match ($billing) {
            'subscription' => __('subscription'),
            'paid_api' => __('paid API'),
            'free_local' => __('free on this Mac'),
            default => $billing,
        };
        $line = "{$displayName} · {$provider} · {$billingLabel}";
        if (! $available && filled($reason)) {
            $line .= ' — '.$reason;
        }

        return $line;
    }
}
