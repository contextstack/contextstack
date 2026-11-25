<?php

namespace App\Native;

use App\Client\Json;

/**
 * The model choice in three steps (CLIENT-API.md §4.5): a provider, one of its models, then the harness that runs
 * it when more than one can (Claude Code, Cursor, Codex CLI, ContextStack's own harness…). A harness is never shown
 * as a model. What is sent is the chosen pair's `value`, the exact string the Director gave; the client never builds
 * a model name. Role-only models (`internal`) and unavailable models or harnesses are left out.
 *
 * Reads `catalog` from GET models. A Director without it gets today's two steps over the flat `models` list, each
 * model with a single, unnamed harness.
 */
final class ModelPicker
{
    /**
     * @param  array<string, array<string, array<string, string>>>  $choices  provider words => model words => harness words => value
     * @param  array<string, string>  $where  value => where it runs and how it is billed, as words
     */
    private function __construct(public readonly array $choices, public readonly array $where, public readonly string $default) {}

    /**
     * @param  array<string, mixed>  $answer  GET models
     */
    public static function fromCatalog(array $answer, bool $leadsOnly = false): self
    {
        $catalog = Json::record($answer['catalog'] ?? null);

        if ($catalog === []) {
            return self::flat($answer, $leadsOnly);
        }

        // Which values can lead, from the flat rows (CLIENT-API.md §11.1a); a pair whose value has no flat row
        // follows its model's `lead`.
        $leads = [];
        foreach (Json::records($answer['models'] ?? null) as $row) {
            $leads[Json::string($row['name'] ?? null)] = ($row['lead'] ?? false) === true;
        }

        return self::structured($catalog, $leadsOnly, $leads);
    }

    /**
     * @return list<string>
     */
    public function providerChoices(): array
    {
        return array_keys($this->choices);
    }

    /**
     * @return list<string>
     */
    public function modelChoices(string $provider): array
    {
        return array_keys($this->choices[$provider] ?? []);
    }

    /**
     * The harnesses that can run a model; a single '' means the catalog names none (the flat list).
     *
     * @return list<string>
     */
    public function harnessChoices(string $provider, string $model): array
    {
        return array_keys($this->choices[$provider][$model] ?? []);
    }

    /**
     * What to send for a provider, model and harness, or '' when the three do not belong together.
     */
    public function value(string $provider, string $model, string $harness): string
    {
        return $this->choices[$provider][$model][$harness] ?? '';
    }

    public function where(string $value): string
    {
        return $this->where[$value] ?? '';
    }

    /**
     * The provider, model and harness to start on: the catalog's default when it is offered, else the first of each.
     *
     * @return array{0: string, 1: string, 2: string}
     */
    public function initial(?string $preferred = null): array
    {
        foreach ([$preferred, $this->default] as $wanted) {
            if ($wanted === null || $wanted === '') {
                continue;
            }
            foreach ($this->choices as $provider => $models) {
                foreach ($models as $model => $harnesses) {
                    $harness = array_search($wanted, $harnesses, true);
                    if (is_string($harness)) {
                        return [$provider, $model, $harness];
                    }
                }
            }
        }
        $provider = (string) array_key_first($this->choices);
        $model = $this->firstModel($provider);

        return [$provider, $model, $this->firstHarness($provider, $model)];
    }

    public function firstModel(string $provider): string
    {
        return (string) array_key_first($this->choices[$provider] ?? []);
    }

    public function firstHarness(string $provider, string $model): string
    {
        return (string) array_key_first($this->choices[$provider][$model] ?? []);
    }

    /**
     * @param  array<string, mixed>  $catalog
     * @param  array<string, bool>  $leads  value => whether it can host a lead
     */
    private static function structured(array $catalog, bool $leadsOnly, array $leads = []): self
    {
        $providerWords = [];
        foreach (Json::records($catalog['providers'] ?? null) as $provider) {
            $id = Json::string($provider['id'] ?? null);
            $providerWords[$id] = Json::string($provider['label'] ?? null, $id);
        }
        $harnesses = [];
        foreach (Json::records($catalog['harnesses'] ?? null) as $harness) {
            $harnesses[Json::string($harness['id'] ?? null)] = $harness;
        }

        $choices = [];
        $where = [];
        foreach (Json::records($catalog['models'] ?? null) as $model) {
            if (($model['internal'] ?? false) === true || ($model['available'] ?? true) === false || ($leadsOnly && ($model['lead'] ?? false) !== true)) {
                continue;
            }
            $providerId = Json::string($model['provider'] ?? null);
            $provider = $providerWords[$providerId] ?? ($providerId !== '' ? $providerId : 'Other');
            $words = Json::string($model['label'] ?? null, Json::string($model['id'] ?? null));
            foreach (Json::records($model['harnesses'] ?? null) as $pair) {
                $harnessId = Json::string($pair['harness'] ?? null);
                $value = Json::string($pair['value'] ?? null);
                $harness = $harnesses[$harnessId] ?? [];
                if ($value === '' || Json::string($harness['unavailable_reason'] ?? null) !== '') {
                    continue;
                }
                if ($leadsOnly && ($leads[$value] ?? true) === false) {
                    continue;
                }
                $harnessWords = Json::string($harness['label'] ?? null, $harnessId);
                $choices[$provider][$words][$harnessWords] = $value;
                $where[$value] = self::describe($harnessWords, Json::string($harness['billing'] ?? null, Json::string($model['billing'] ?? null)), Json::strings($harness['nodes'] ?? null));
            }
        }
        ksort($choices, SORT_NATURAL | SORT_FLAG_CASE);

        return new self($choices, $where, Json::string(Json::record($catalog['default'] ?? null)['value'] ?? null));
    }

    /**
     * @param  array<string, mixed>  $answer
     */
    private static function flat(array $answer, bool $leadsOnly): self
    {
        $choices = [];
        $where = [];
        foreach (Json::records($answer['models'] ?? null) as $row) {
            $name = Json::string($row['name'] ?? null);
            if ($name === '' || ($row['internal'] ?? false) === true || ($row['available'] ?? true) === false || ($leadsOnly && ($row['lead'] ?? false) !== true)) {
                continue;
            }
            $provider = Json::string($row['provider_label'] ?? null, Json::string($row['provider'] ?? null, 'Other'));
            $label = Json::string($row['label'] ?? null, $name);
            $words = isset($choices[$provider][$label]) ? $label.' ('.$name.')' : $label;
            $choices[$provider][$words][''] = $name;
            $nodes = Json::strings($row['nodes'] ?? null);
            $where[$name] = $nodes !== [] ? 'Runs on '.implode(', ', $nodes) : 'No machine reports it yet';
        }
        ksort($choices, SORT_NATURAL | SORT_FLAG_CASE);

        return new self($choices, $where, Json::string($answer['default_model'] ?? null));
    }

    /**
     * @param  list<string>  $nodes
     */
    private static function describe(string $harness, string $billing, array $nodes): string
    {
        $billingWords = match ($billing) {
            'subscription' => 'subscription',
            'paid_api' => 'pay as you go',
            'free_local' => 'local, free',
            'hosted' => 'hosted',
            'provider' => 'billed by the provider',
            default => '',
        };

        return implode(' · ', array_filter([
            'Runs in '.$harness,
            $billingWords,
            $nodes !== [] ? 'on '.implode(', ', $nodes) : 'no machine reports it yet',
        ]));
    }
}
