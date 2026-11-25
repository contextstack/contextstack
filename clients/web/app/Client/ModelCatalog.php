<?php

declare(strict_types=1);

namespace App\Client;

/**
 * Director GET models (CLIENT-API.md §4.5): the flat `models` rows, for the failed-turn model switch and for the
 * two-step provider-then-model picker the new-lane and new-conversation pages fall back to when the answer has no
 * `catalog` (HarnessCatalog builds the three-step picker from it). Role-only rows (`internal`) are never offered.
 *
 * @phpstan-type Choice array{
 *     name: string,
 *     label: string,
 *     available: bool,
 *     display_name: string,
 *     provider: string,
 *     billing: string,
 *     reason: string|null,
 *     lead: bool,
 *     model_label: string,
 *     provider_label: string,
 *     nodes: list<string>,
 *     option: string,
 * }
 * @phpstan-type ProviderGroup array{provider: string, label: string, ready: bool, models: list<Choice>}
 */
final class ModelCatalog
{
    public const string NoReadyHint = 'Sign in to a subscription CLI (for example `claude`) or add an API key (`cs init --api-key openai`), then reload this page.';

    /**
     * Each row once, in the catalog's order, without role-only (`internal`) rows. `label` is the one-line text for a
     * flat list; `option` is the text inside a provider group (the model's catalog label, billing, why it is not
     * ready, and the machines that report support).
     *
     * @param  array<string, mixed>  $catalog
     * @return array{choices: list<Choice>, default: string, canStart: bool}
     */
    public static function parse(array $catalog): array
    {
        $choices = [];
        foreach ((array) ($catalog['legacy_models'] ?? $catalog['models'] ?? []) as $row) {
            if (! is_array($row) || ! is_string($row['name'] ?? null) || ($row['internal'] ?? false) === true) {
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
                : self::displayNameFromLabel(is_string($row['label'] ?? null) ? $row['label'] : $name);
            $provider = is_string($row['provider'] ?? null) && $row['provider'] !== ''
                ? $row['provider']
                : 'local';
            $available = (bool) ($row['available'] ?? true);
            $reason = is_string($row['unavailable_reason'] ?? null) ? $row['unavailable_reason'] : null;
            // Whether a hosted lead can run on it: the catalog always says.
            $lead = ($row['lead'] ?? false) === true;
            $modelLabel = is_string($row['label'] ?? null) && $row['label'] !== '' ? $row['label'] : $displayName;
            $providerLabel = is_string($row['provider_label'] ?? null) && $row['provider_label'] !== '' ? $row['provider_label'] : $provider;
            $nodes = array_values(array_filter((array) ($row['nodes'] ?? []), fn (mixed $node): bool => is_string($node) && $node !== ''));

            $choices[] = [
                'name' => $name,
                'label' => self::optionLabel($displayName, $provider, $billing, $available, $reason),
                'available' => $available,
                'display_name' => $displayName,
                'provider' => $provider,
                'billing' => $billing,
                'reason' => $reason,
                'lead' => $lead,
                'model_label' => $modelLabel,
                'provider_label' => $providerLabel,
                'nodes' => $nodes,
                'option' => self::groupedOptionLabel($modelLabel, $billing, $available, $reason, $nodes),
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
     * The choices grouped by provider, in the order each provider first appears. A group is ready when one of its
     * models is.
     *
     * @param  list<Choice>  $choices
     * @return list<ProviderGroup>
     */
    public static function groups(array $choices): array
    {
        $groups = [];
        foreach ($choices as $choice) {
            $provider = $choice['provider'];
            $groups[$provider] ??= ['provider' => $provider, 'label' => $choice['provider_label'], 'ready' => false, 'models' => []];
            $groups[$provider]['models'][] = $choice;
            $groups[$provider]['ready'] = $groups[$provider]['ready'] || $choice['available'];
        }

        return array_values($groups);
    }

    /**
     * The provider group a model belongs to, or '' when the model is not among the choices.
     *
     * @param  list<Choice>  $choices
     */
    public static function providerOf(string $name, array $choices): string
    {
        foreach ($choices as $choice) {
            if ($choice['name'] === $name) {
                return $choice['provider'];
            }
        }

        return '';
    }

    /**
     * The model to select when a person picks a provider: its first ready model that the machine is not out of quota
     * for, else its first ready model, else '' (none of its models is ready).
     *
     * @param  list<Choice>  $choices
     * @param  array<string, string|null>  $limited  model name → reset time, for the machine the model would run on
     */
    public static function firstReadyOf(string $provider, array $choices, array $limited = []): string
    {
        $ready = [];
        foreach ($choices as $choice) {
            if ($choice['provider'] === $provider && $choice['available']) {
                $ready[] = $choice['name'];
            }
        }
        if ($ready === []) {
            return '';
        }

        return ProviderLimits::prefer($ready[0], $ready, $limited);
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

    /**
     * @param  list<string>  $nodes
     */
    private static function groupedOptionLabel(string $modelLabel, string $billing, bool $available, ?string $reason, array $nodes): string
    {
        $line = $modelLabel.' · '.self::billingLabel($billing);
        if (! $available && filled($reason)) {
            $line .= ' — '.$reason;
        }
        if ($nodes !== []) {
            $line .= ' · '.__('runs on :machines', ['machines' => implode(', ', $nodes)]);
        }

        return $line;
    }

    /**
     * Billing in words: "subscription", "paid API", "free on this Mac", "hosted".
     */
    public static function billingLabel(string $billing): string
    {
        return match ($billing) {
            'subscription' => __('subscription'),
            'paid_api' => __('paid API'),
            'hosted' => __('hosted'),
            'free_local' => __('free on this Mac'),
            default => $billing,
        };
    }

    private static function optionLabel(
        string $displayName,
        string $provider,
        string $billing,
        bool $available,
        ?string $reason,
    ): string {
        $line = "{$displayName} · {$provider} · ".self::billingLabel($billing);
        if (! $available && filled($reason)) {
            $line .= ' — '.$reason;
        }

        return $line;
    }
}
