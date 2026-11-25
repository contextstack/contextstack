<?php

declare(strict_types=1);

namespace App\Client;

/**
 * The three-step model picker (provider, then model, then the harness that runs it) over the `catalog` object of
 * Director GET models (CLIENT-API.md §4.5). A harness (Claude Code, Codex CLI, ContextStack's own…) is never a
 * model: each model lists the harnesses that can run it, and each (model, harness) pair carries the exact `value`
 * to send as the model. Clients never compose names.
 *
 * When the answer has no `catalog` (a Director before it), fromAnswer() returns null and the pages keep
 * ModelCatalog's two-step picker over the flat `models`.
 *
 * @phpstan-type Pair array{
 *     harness: string,
 *     label: string,
 *     value: string,
 *     billing: string,
 *     nodes: list<string>,
 *     available: bool,
 *     reason: string|null,
 *     lead: bool|null,
 *     option: string,
 * }
 * @phpstan-type Model array{
 *     id: string,
 *     provider: string,
 *     label: string,
 *     available: bool,
 *     reason: string|null,
 *     lead: bool,
 *     option: string,
 *     pairs: list<Pair>,
 * }
 * @phpstan-type Provider array{id: string, label: string, ready: bool}
 * @phpstan-type Catalog array{providers: list<Provider>, models: list<Model>, default: string}
 * @phpstan-type Selection array{provider: string, model: string, harness: string, value: string}
 */
final class HarnessCatalog
{
    /**
     * The picker's catalog, or null when the answer has none. A model's label gains its version in brackets unless the
     * label already spells it. Internal rows are dropped; a pair is ready when its
     * model is and its harness has no unavailable reason. Availability, billing, nodes and the lead flag of a pair
     * come from the flat `models` row named by its value when there is one.
     *
     * @param  array<array-key, mixed>  $answer  a GET models answer
     * @return Catalog|null
     */
    public static function fromAnswer(array $answer): ?array
    {
        $catalog = $answer['catalog'] ?? null;
        if (! is_array($catalog)) {
            return null;
        }

        $flat = [];
        foreach (self::rows($answer['models'] ?? null) as $row) {
            if (is_string($row['name'] ?? null)) {
                $flat[$row['name']] ??= $row;
            }
        }

        $harnesses = [];
        foreach (self::rows($catalog['harnesses'] ?? null) as $row) {
            if (is_string($row['id'] ?? null) && $row['id'] !== '') {
                $harnesses[$row['id']] = $row;
            }
        }

        $models = [];
        foreach (self::rows($catalog['models'] ?? null) as $row) {
            $model = self::model($row, $harnesses, $flat);
            if ($model !== null) {
                $models[] = $model;
            }
        }

        $providers = [];
        foreach (self::rows($catalog['providers'] ?? null) as $row) {
            if (is_string($row['id'] ?? null) && $row['id'] !== '') {
                $providers[$row['id']] = self::text($row['label'] ?? null) ?? $row['id'];
            }
        }

        $default = is_array($catalog['default'] ?? null) ? (self::text($catalog['default']['value'] ?? null) ?? '') : '';

        return self::withProviders(['providers' => [], 'models' => $models, 'default' => $default], $providers);
    }

    /**
     * Only what can host a lead conversation: models with `lead: true`, and of those only the pairs not known to be
     * unable to lead (a pair whose lead ability the data does not state is kept).
     *
     * @param  Catalog  $catalog
     * @return Catalog
     */
    public static function forLead(array $catalog): array
    {
        $models = [];
        foreach ($catalog['models'] as $model) {
            if (! $model['lead']) {
                continue;
            }
            $pairs = array_values(array_filter($model['pairs'], fn (array $pair): bool => $pair['lead'] !== false));
            if ($pairs !== []) {
                $models[] = self::withPairs($model, $pairs);
            }
        }
        $labels = array_column($catalog['providers'], 'label', 'id');

        return self::withProviders(['providers' => [], 'models' => $models, 'default' => $catalog['default']], $labels);
    }

    /**
     * One provider's models, in the catalog's order.
     *
     * @param  Catalog  $catalog
     * @return list<Model>
     */
    public static function modelsOf(array $catalog, string $provider): array
    {
        return array_values(array_filter($catalog['models'], fn (array $model): bool => $model['provider'] === $provider));
    }

    /**
     * The harnesses that can run one model (all of them, the unready ones too, for the picker to show disabled).
     *
     * @param  Catalog  $catalog
     * @return list<Pair>
     */
    public static function pairsOf(array $catalog, string $provider, string $model): array
    {
        foreach (self::modelsOf($catalog, $provider) as $row) {
            if ($row['id'] === $model) {
                return $row['pairs'];
            }
        }

        return [];
    }

    /**
     * Every ready pair's value: what a form may send.
     *
     * @param  Catalog  $catalog
     * @return list<string>
     */
    public static function readyValues(array $catalog): array
    {
        $values = [];
        foreach ($catalog['models'] as $model) {
            foreach ($model['pairs'] as $pair) {
                if ($pair['available'] && ! in_array($pair['value'], $values, true)) {
                    $values[] = $pair['value'];
                }
            }
        }

        return $values;
    }

    /**
     * The value to start on: the catalog default when it is ready here, else the first ready pair, preferring one
     * the machine is not out of quota for. '' when nothing is ready.
     *
     * @param  Catalog  $catalog
     * @param  array<string, string|null>  $limited  value → reset time, for the machine it would run on
     */
    public static function defaultValue(array $catalog, array $limited = []): string
    {
        $ready = self::readyValues($catalog);
        if ($ready === []) {
            return '';
        }
        $start = in_array($catalog['default'], $ready, true) ? $catalog['default'] : $ready[0];

        return ProviderLimits::prefer($start, $ready, $limited);
    }

    /**
     * Settle the three steps after one of them changed: keep the provider, keep the model when it belongs to that
     * provider (else take its first model with a ready pair in quota, else its first ready model), keep the harness
     * when it is ready for that model (else the catalog default's, else the first ready pair in quota). The value is
     * '' when the model has no ready pair.
     *
     * @param  Catalog  $catalog
     * @param  array<string, string|null>  $limited  value → reset time, for the machine it would run on
     * @return Selection
     */
    public static function choose(array $catalog, string $provider, string $model = '', string $harness = '', array $limited = []): array
    {
        $models = self::modelsOf($catalog, $provider);
        $chosen = null;
        foreach ($models as $row) {
            if ($row['id'] === $model) {
                $chosen = $row;
            }
        }
        $chosen ??= self::firstOf($models, fn (array $row): bool => self::pickPair($row['pairs'], '', $catalog['default'], $limited, true) !== null)
            ?? self::firstOf($models, fn (array $row): bool => $row['available'])
            ?? ($models[0] ?? null);
        if ($chosen === null) {
            return ['provider' => $provider, 'model' => '', 'harness' => '', 'value' => ''];
        }
        $pair = self::pickPair($chosen['pairs'], $harness, $catalog['default'], $limited, false);

        return ['provider' => $provider, 'model' => $chosen['id'], 'harness' => $pair['harness'] ?? '', 'value' => $pair['value'] ?? ''];
    }

    /**
     * The three steps that send $value, or null when no pair sends it.
     *
     * @param  Catalog  $catalog
     * @return Selection|null
     */
    public static function locate(array $catalog, string $value): ?array
    {
        foreach ($catalog['models'] as $model) {
            foreach ($model['pairs'] as $pair) {
                if ($pair['value'] === $value) {
                    return ['provider' => $model['provider'], 'model' => $model['id'], 'harness' => $pair['harness'], 'value' => $value];
                }
            }
        }

        return null;
    }

    /**
     * "Runs in Claude Code · subscription · on node-a": the line shown when only one harness can run the model.
     *
     * @param  Pair  $pair
     */
    public static function runsIn(array $pair): string
    {
        return __('Runs in :harness', ['harness' => $pair['option']]);
    }

    /**
     * @param  array<array-key, mixed>  $row
     * @param  array<string, array<array-key, mixed>>  $harnesses
     * @param  array<string, array<array-key, mixed>>  $flat
     * @return Model|null
     */
    private static function model(array $row, array $harnesses, array $flat): ?array
    {
        $id = self::text($row['id'] ?? null);
        if ($id === null || ($row['internal'] ?? false) === true) {
            return null;
        }
        $provider = self::text($row['provider'] ?? null) ?? 'local';
        $modelAvailable = ($row['available'] ?? true) !== false;
        $modelReason = self::text($row['unavailable_reason'] ?? null);

        $pairs = [];
        foreach (self::rows($row['harnesses'] ?? null) as $entry) {
            $harnessId = self::text($entry['harness'] ?? null);
            $value = self::text($entry['value'] ?? null);
            if ($harnessId === null || $value === null) {
                continue;
            }
            $harness = $harnesses[$harnessId] ?? [];
            $source = $flat[$value] ?? $row;
            $harnessReason = self::text($harness['unavailable_reason'] ?? null);
            $available = $harnessReason === null && ($source['available'] ?? $modelAvailable) !== false;
            $reason = $available ? null : ($harnessReason ?? self::text($source['unavailable_reason'] ?? null) ?? $modelReason);
            $billing = self::text($harness['billing'] ?? null);
            if ($billing === null || $billing === 'provider') {
                $billing = self::text($source['billing'] ?? null) ?? self::text($row['billing'] ?? null) ?? '';
            }
            $nodes = isset($flat[$value]) ? self::strings($flat[$value]['nodes'] ?? null) : [];
            $nodes = $nodes !== [] ? $nodes : self::strings($harness['nodes'] ?? null);
            $label = self::text($harness['label'] ?? null) ?? $harnessId;

            $pairs[] = [
                'harness' => $harnessId,
                'label' => $label,
                'value' => $value,
                'billing' => $billing,
                'nodes' => $nodes,
                'available' => $available,
                'reason' => $reason,
                'lead' => isset($flat[$value]) && is_bool($flat[$value]['lead'] ?? null) ? $flat[$value]['lead'] : null,
                'option' => self::pairOption($label, $billing, $available, $reason, $nodes),
            ];
        }
        if ($pairs === []) {
            return null;
        }

        $label = self::text($row['label'] ?? null) ?? $id;
        $version = self::text($row['version'] ?? null);
        if ($version !== null && ! str_contains(self::squashed($label), self::squashed($version))) {
            $label .= ' ('.$version.')';
        }

        return self::withPairs([
            'id' => $id,
            'provider' => $provider,
            'label' => $label,
            'available' => false,
            'reason' => $modelReason,
            'lead' => ($row['lead'] ?? false) === true,
            'option' => '',
            'pairs' => [],
        ], $pairs);
    }

    /**
     * The model with these pairs, its readiness and option text worked out from them.
     *
     * @param  Model  $model
     * @param  list<Pair>  $pairs
     * @return Model
     */
    private static function withPairs(array $model, array $pairs): array
    {
        $available = array_filter($pairs, fn (array $pair): bool => $pair['available']) !== [];
        $reason = $available ? null : ($model['reason'] ?? ($pairs[0]['reason'] ?? null));
        $option = $model['label'];
        if (! $available && filled($reason)) {
            $option .= ' — '.$reason;
        }

        return [...$model, 'available' => $available, 'reason' => $reason, 'option' => $option, 'pairs' => $pairs];
    }

    /**
     * The providers that have a model, in the catalog's provider order (a model's provider the list leaves out comes
     * last, under its id).
     *
     * @param  Catalog  $catalog
     * @param  array<array-key, string>  $labels  provider id → label
     * @return Catalog
     */
    private static function withProviders(array $catalog, array $labels): array
    {
        $ready = [];
        foreach ($catalog['models'] as $model) {
            $ready[$model['provider']] = ($ready[$model['provider']] ?? false) || $model['available'];
        }
        $providers = [];
        foreach ($labels as $id => $label) {
            $id = (string) $id;
            if (array_key_exists($id, $ready)) {
                $providers[] = ['id' => $id, 'label' => $label, 'ready' => $ready[$id]];
            }
        }
        foreach ($ready as $id => $isReady) {
            if (! array_key_exists($id, $labels)) {
                $providers[] = ['id' => (string) $id, 'label' => (string) $id, 'ready' => $isReady];
            }
        }

        return [...$catalog, 'providers' => $providers];
    }

    /**
     * The pair to select: $harness when it is ready, else the catalog default's, else the first ready one in quota,
     * else the first ready one (unless $inQuotaOnly), else null.
     *
     * @param  list<Pair>  $pairs
     * @param  array<string, string|null>  $limited
     * @return Pair|null
     */
    private static function pickPair(array $pairs, string $harness, string $default, array $limited, bool $inQuotaOnly): ?array
    {
        $ready = array_values(array_filter($pairs, fn (array $pair): bool => $pair['available']));
        $inQuota = array_values(array_filter($ready, fn (array $pair): bool => ! array_key_exists($pair['value'], $limited)));

        return self::firstOf($ready, fn (array $pair): bool => $pair['harness'] === $harness)
            ?? self::firstOf($inQuota, fn (array $pair): bool => $pair['value'] === $default)
            ?? ($inQuota[0] ?? null)
            ?? ($inQuotaOnly ? null : ($ready[0] ?? null));
    }

    /**
     * @template T of array<string, mixed>
     *
     * @param  list<T>  $rows
     * @param  callable(T): bool  $matches
     * @return T|null
     */
    private static function firstOf(array $rows, callable $matches): ?array
    {
        foreach ($rows as $row) {
            if ($matches($row)) {
                return $row;
            }
        }

        return null;
    }

    /**
     * @param  list<string>  $nodes
     */
    private static function pairOption(string $label, string $billing, bool $available, ?string $reason, array $nodes): string
    {
        $line = $billing === '' ? $label : $label.' · '.ModelCatalog::billingLabel($billing);
        if ($nodes !== []) {
            $line .= ' · '.__('on :machines', ['machines' => implode(', ', $nodes)]);
        }
        if (! $available && filled($reason)) {
            $line .= ' — '.$reason;
        }

        return $line;
    }

    /**
     * @return list<array<array-key, mixed>>
     */
    private static function rows(mixed $list): array
    {
        return is_array($list) ? array_values(array_filter($list, 'is_array')) : [];
    }

    /**
     * @return list<string>
     */
    private static function strings(mixed $list): array
    {
        return is_array($list) ? array_values(array_filter($list, fn (mixed $item): bool => is_string($item) && $item !== '')) : [];
    }

    /**
     * Lower case letters and digits only, so "Claude Sonnet 5.5" and "claude-sonnet-5-5" compare equal.
     */
    private static function squashed(string $text): string
    {
        return (string) preg_replace('/[^a-z0-9]/', '', mb_strtolower($text));
    }

    private static function text(mixed $value): ?string
    {
        return is_string($value) && $value !== '' ? $value : null;
    }
}
