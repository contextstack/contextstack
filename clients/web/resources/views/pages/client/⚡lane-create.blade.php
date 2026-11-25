<?php

use App\Client\DirectorClient;
use App\Client\HarnessCatalog;
use App\Client\ModelCatalog;
use App\Client\ProviderLimits;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Attributes\Url;
use Illuminate\Support\Str;
use Illuminate\Validation\Rule;
use Livewire\Component;

/*
 * Start a coding lane: the Director's built-in coding lane (`POST intents` with `lane`, CLIENT-API "Built-in coding
 * lane intents"), the same definition `cs mcp start_task` uses. The Director writes the spec, system prompt, goal and
 * check; the app sends the title, the goal, the model, and the repo and lane name (or neither, for scratch work).
 * The model defaults to `auto`: the Director plans first and picks the coding model from the plan. Auto applies to
 * repo lanes only, so a scratch lane sends the catalog's default model instead. An explicitly chosen model may ask
 * for a read-only planning job first (`lane.plan_with`), again for repo lanes only.
 * The model is picked in three steps: Auto or a provider first, then one of that provider's models, then (when more
 * than one can run it) the harness that runs it (HarnessCatalog); the pair's `value` is sent as the model. A Director
 * whose GET models has no `catalog` gets the two-step picker over its flat models (ModelCatalog::groups).
 * A lane may belong to a project, and then carry the project's active decision log as context
 * (`lane.context: ["decision_log"]` with the intent's `project_id`): the Director freezes a copy into the job.
 */
new #[Layout('layouts::client')] #[Title('New task lane')] class extends Component
{
    /** Model selector the Director reads as "plan first, then pick the coding model from the plan". */
    public const AUTO = 'auto';

    /** Planners offered for "Plan first" (CLI executor templates). */
    public const PLANNERS = ['codex', 'claude-code-sonnet'];

    public string $node = '';

    public string $title = '';

    public string $goal = '';

    /** What is sent: `auto` (plan first, then let the Director pick), or the chosen pair's value (a model name). */
    public string $model = self::AUTO;

    /** The picker's first step: `auto`, or the provider group of the chosen model. */
    public string $provider = self::AUTO;

    /** The picker's second step with a catalog: the catalog model id. */
    public string $catalogModel = '';

    /** The picker's third step with a catalog: the harness that runs the model. */
    public string $harness = '';

    /**
     * The three-step picker's catalog (HarnessCatalog::fromAnswer), or null for the two-step fallback.
     *
     * @var array{providers: list<array{id: string, label: string, ready: bool}>, models: list<array{id: string, provider: string, label: string, available: bool, reason: string|null, lead: bool, option: string, pairs: list<array{harness: string, label: string, value: string, billing: string, nodes: list<string>, available: bool, reason: string|null, lead: bool|null, option: string}>}>, default: string}|null
     */
    #[Locked]
    public ?array $catalog = null;

    /** CLI executor for a planning job before an explicitly chosen model's coding job; '' for none. */
    public string $planWith = '';

    /** The catalog's default model, preferring one in quota: what a scratch lane runs in place of Auto. */
    #[Locked]
    public string $defaultModel = '';

    public string $repo = '';

    public string $lane = '';

    public bool $scratch = false;

    #[Url(as: 'project', except: '')]
    public string $projectId = '';

    /** Attach the project's active decision log to the lane. */
    public bool $decisionLog = false;

    /** @var array<string, string> project id => name */
    #[Locked]
    public array $projects = [];

    #[Locked]
    public string $submitKey = '';

    /** @var list<string> */
    #[Locked]
    public array $nodes = [];

    /**
     * Each machine's out-of-quota models (ProviderLimits::fromNodes): node → model → reset time.
     *
     * @var array<string, array<string, string|null>>
     */
    #[Locked]
    public array $limits = [];

    /** @var list<array{name: string, label: string, available: bool, display_name: string, provider: string, billing: string, reason: string|null, lead: bool, model_label: string, provider_label: string, nodes: list<string>, option: string}> */
    #[Locked]
    public array $modelChoices = [];

    /** @var list<string> */
    #[Locked]
    public array $repos = [];

    public string $problem = '';

    /** @var array{kind: string, id: string}|array{} */
    #[Locked]
    public array $conflict = [];

    public function mount(DirectorClient $director): void
    {
        $this->submitKey = DirectorClient::newKey();
        try {
            $nodes = $director->get('nodes');
            $this->nodes = array_values(array_map(fn (array $node): string => (string) $node['name'],
                array_filter((array) ($nodes['nodes'] ?? []), fn ($node): bool => is_array($node) && ($node['revoked_at'] ?? null) === null)));
            $this->node = $this->nodes[0] ?? '';
            $this->limits = ProviderLimits::fromNodes($nodes);

            $catalog = $director->get('models');
            $this->modelChoices = ModelCatalog::parse($catalog)['choices'];
            $this->catalog = HarnessCatalog::fromAnswer($catalog);
            if ($this->catalog !== null) {
                $this->defaultModel = HarnessCatalog::defaultValue($this->catalog, $this->limitedHere());
            } else {
                $default = is_string($catalog['default_model'] ?? null) ? $catalog['default_model'] : '';
                $default = $default !== '' ? $default : ($this->modelChoices[0]['name'] ?? 'local-coder');
                $ready = array_values(array_column(array_filter($this->modelChoices, fn (array $choice): bool => $choice['available']), 'name'));
                $this->defaultModel = ProviderLimits::prefer($default, $ready, $this->limitedHere());
            }

            $reposAnswer = $director->get('repos');
            foreach ((array) ($reposAnswer['repos'] ?? []) as $row) {
                if (is_array($row) && is_string($row['name'] ?? null)) {
                    $this->repos[] = $row['name'];
                }
            }
            $this->repo = $this->repos[0] ?? '';

            foreach ((array) ($director->get('projects')['projects'] ?? []) as $project) {
                if (is_array($project) && is_string($project['project_id'] ?? null)) {
                    $this->projects[$project['project_id']] = (string) ($project['name'] ?? $project['project_id']);
                }
            }
            if ($this->projectId !== '' && ! isset($this->projects[$this->projectId])) {
                $this->projectId = '';
            }
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();
        }
    }

    public function updatedTitle(): void
    {
        if ($this->lane === '' && $this->title !== '') {
            // A suggestion the person can change; the Director checks the name.
            $this->lane = Str::limit(Str::slug($this->title), 63, '');
        }
    }

    /** Auto plans against a repo, so a scratch lane switches to the catalog's default model. */
    public function updatedScratch(): void
    {
        if ($this->scratch && $this->model === self::AUTO) {
            $this->model = $this->defaultModel;
            $this->updatedModel();
        }
    }

    /**
     * Picking Auto sends `auto`; picking a provider selects its first ready model (and that model's harness),
     * preferring one in quota.
     */
    public function updatedProvider(): void
    {
        if ($this->provider === self::AUTO) {
            $this->model = self::AUTO;
            $this->catalogModel = '';
            $this->harness = '';
        } elseif ($this->catalog !== null) {
            $this->select(HarnessCatalog::choose($this->catalog, $this->provider, limited: $this->limitedHere()));
        } else {
            $this->model = ModelCatalog::firstReadyOf($this->provider, $this->modelChoices, $this->limitedHere());
        }
    }

    /** Picking a catalog model selects the harness to run it: the catalog default's, else the first ready one. */
    public function updatedCatalogModel(): void
    {
        if ($this->catalog !== null) {
            $this->select(HarnessCatalog::choose($this->catalog, $this->provider, $this->catalogModel, limited: $this->limitedHere()));
        }
    }

    public function updatedHarness(): void
    {
        if ($this->catalog !== null) {
            $this->select(HarnessCatalog::choose($this->catalog, $this->provider, $this->catalogModel, $this->harness, $this->limitedHere()));
        }
    }

    /** A model set directly (not through the steps) moves the steps to the pair or group that sends it. */
    public function updatedModel(): void
    {
        if ($this->model === self::AUTO) {
            $this->provider = self::AUTO;
            $this->catalogModel = '';
            $this->harness = '';
        } elseif ($this->catalog !== null) {
            $found = HarnessCatalog::locate($this->catalog, $this->model);
            if ($found !== null) {
                $this->select($found);
            }
        } else {
            $this->provider = ModelCatalog::providerOf($this->model, $this->modelChoices);
        }
    }

    /**
     * @param  array{provider: string, model: string, harness: string, value: string}  $selection
     */
    private function select(array $selection): void
    {
        $this->provider = $selection['provider'];
        $this->catalogModel = $selection['model'];
        $this->harness = $selection['harness'];
        $this->model = $selection['value'];
    }

    /**
     * The chosen machine's out-of-quota models: value → reset time.
     *
     * @return array<string, string|null>
     */
    public function limitedHere(): array
    {
        return $this->limits[$this->node] ?? [];
    }

    /**
     * Provider groups for the picker.
     *
     * @return list<array{provider: string, label: string, ready: bool, models: list<array{name: string, label: string, available: bool, display_name: string, provider: string, billing: string, reason: string|null, lead: bool, model_label: string, provider_label: string, nodes: list<string>, option: string}>}>
     */
    public function modelGroups(): array
    {
        return ModelCatalog::groups($this->modelChoices);
    }

    /**
     * The chosen provider's catalog models, for the second step.
     *
     * @return list<array{id: string, provider: string, label: string, available: bool, reason: string|null, lead: bool, option: string, pairs: list<array{harness: string, label: string, value: string, billing: string, nodes: list<string>, available: bool, reason: string|null, lead: bool|null, option: string}>}>
     */
    public function catalogModels(): array
    {
        return $this->catalog === null ? [] : HarnessCatalog::modelsOf($this->catalog, $this->provider);
    }

    /**
     * The harnesses that can run the chosen catalog model, for the third step.
     *
     * @return list<array{harness: string, label: string, value: string, billing: string, nodes: list<string>, available: bool, reason: string|null, lead: bool|null, option: string}>
     */
    public function catalogPairs(): array
    {
        return $this->catalog === null ? [] : HarnessCatalog::pairsOf($this->catalog, $this->provider, $this->catalogModel);
    }

    public function submit(DirectorClient $director): void
    {
        if ($this->scratch && $this->model === self::AUTO) {
            $this->model = $this->defaultModel;
            $this->updatedModel();
        }

        $rules = [
            'node' => ['required', 'string', 'max:128'],
            'title' => ['required', 'string', 'max:200'],
            'goal' => ['required', 'string', 'max:200000'],
            // A catalog pair's value is sent exactly as the catalog gives it (it may hold `:` or `/`).
            'model' => $this->catalog !== null
                ? ['required', 'string', Rule::in([...($this->scratch ? [] : [self::AUTO]), ...HarnessCatalog::readyValues($this->catalog)])]
                : ['required', 'string', 'max:128', 'regex:/^[a-z0-9][a-z0-9._-]{0,62}$/'],
            'planWith' => ['nullable', 'string', Rule::in(self::PLANNERS)],
            'projectId' => ['nullable', 'string', 'max:128'],
        ];
        if (! $this->scratch) {
            $rules['repo'] = ['required', 'string', 'max:128', 'regex:/^[a-z0-9][a-z0-9._-]{0,62}$/'];
            $rules['lane'] = ['required', 'string', 'max:128', 'regex:/^[a-z0-9][a-z0-9._-]{0,62}$/'];
        }
        $this->validate($rules);

        $this->problem = '';
        $this->conflict = [];

        $body = [
            'node' => $this->node,
            'lane' => array_filter([
                'title' => $this->title,
                'goal' => $this->goal,
                'model' => $this->model,
                'repo' => $this->scratch ? null : $this->repo,
                'lane' => $this->scratch ? null : $this->lane,
                'plan_with' => $this->scratch || $this->model === self::AUTO || $this->planWith === '' ? null : $this->planWith,
                'context' => $this->projectId !== '' && $this->decisionLog ? ['decision_log'] : null,
            ], fn ($value) => $value !== null),
            'meta' => ['source' => 'client-web-lane'],
        ];
        if ($this->projectId !== '') {
            $body['project_id'] = $this->projectId;
        }

        try {
            $answer = $director->write('intents', $body, $this->submitKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Starting again is safe; it replays the same request.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $record = (array) ($refused->body['record'] ?? []);
                $this->conflict = ['kind' => (string) ($record['kind'] ?? ''), 'id' => (string) ($record['id'] ?? '')];
                $this->submitKey = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return;
        }

        $this->submitKey = DirectorClient::newKey();
        $job = (array) (($answer->body['jobs'] ?? [])[0] ?? []);
        $jobId = (string) ($job['job_id'] ?? '');
        if ($jobId !== '') {
            $this->redirectRoute('lanes.show', ['job' => $jobId], navigate: true);
        } else {
            $this->redirectRoute('lanes.index', navigate: true);
        }
    }
};
?>

<section class="mx-auto flex w-full max-w-3xl flex-col gap-5" data-test="lane-create-page">
    <div>
        <flux:heading size="lg" level="1" class="text-lg!">{{ __('New task lane') }}</flux:heading>
        <flux:text class="mt-0.5">{{ __('Start a coding agent on a repo lane or in a scratch workspace.') }}</flux:text>
    </div>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
            @if (($conflict['kind'] ?? '') === 'intent' && $conflict['id'] !== '')
                <x-slot name="actions">
                    <flux:button size="sm" :href="route('intents.show', $conflict['id'])" wire:navigate>{{ __('Open the recorded task') }}</flux:button>
                </x-slot>
            @endif
        </flux:callout>
    @endif

    <form wire:submit="submit" class="space-y-6">
        <div class="grid gap-4 sm:grid-cols-2">
            <flux:select wire:model.live="node" :label="__('Machine')">
                @foreach ($nodes as $name)
                    <flux:select.option :value="$name" wire:key="node-{{ $name }}">{{ $name }}</flux:select.option>
                @endforeach
            </flux:select>
            @php($groups = $this->modelGroups())
            <flux:select wire:model.live="provider" :label="__('Provider')" data-test="lane-provider">
                <flux:select.option :value="$this::AUTO" wire:key="provider-auto" :disabled="$scratch">{{ __('Auto (plans first, then picks the model)') }}</flux:select.option>
                @if ($catalog !== null)
                    @foreach ($catalog['providers'] as $catalogProvider)
                        <flux:select.option :value="$catalogProvider['id']" wire:key="provider-{{ $catalogProvider['id'] }}">{{ $catalogProvider['label'] }}@unless ($catalogProvider['ready']) — {{ __('not ready') }}@endunless</flux:select.option>
                    @endforeach
                @else
                    @foreach ($groups as $group)
                        <flux:select.option :value="$group['provider']" wire:key="provider-{{ $group['provider'] }}">{{ $group['label'] }}@unless ($group['ready']) — {{ __('not ready') }}@endunless</flux:select.option>
                    @endforeach
                @endif
            </flux:select>
        </div>

        @if ($provider !== $this::AUTO && $catalog !== null)
            @php($pairs = $this->catalogPairs())
            <div class="grid gap-4 sm:grid-cols-2">
                <flux:select wire:model.live="catalogModel" :label="__('Model')" :placeholder="$catalogModel === '' ? __('No model from this provider') : null" data-test="lane-model">
                    @foreach ($this->catalogModels() as $catalogChoice)
                        <flux:select.option :value="$catalogChoice['id']" wire:key="model-{{ $provider }}-{{ $catalogChoice['id'] }}" :disabled="! $catalogChoice['available']">{{ $catalogChoice['option'] }}</flux:select.option>
                    @endforeach
                </flux:select>
                @if (count($pairs) > 1)
                    <flux:select wire:model.live="harness" :label="__('Harness')" :placeholder="$harness === '' ? __('No ready harness for this model') : null" data-test="lane-harness">
                        @foreach ($pairs as $pair)
                            <flux:select.option :value="$pair['harness']" wire:key="harness-{{ $catalogModel }}-{{ $pair['harness'] }}" :disabled="! $pair['available']">{{ $pair['option'] }}@if (array_key_exists($pair['value'], $limits[$node] ?? [])) — {{ \App\Client\ProviderLimits::phrase($limits[$node][$pair['value']]) }}@endif</flux:select.option>
                        @endforeach
                    </flux:select>
                @elseif (count($pairs) === 1)
                    <flux:text class="text-sm text-zinc-600 sm:pt-8 dark:text-zinc-400" data-test="lane-harness-text">{{ \App\Client\HarnessCatalog::runsIn($pairs[0]) }}@if (array_key_exists($pairs[0]['value'], $limits[$node] ?? [])) — {{ \App\Client\ProviderLimits::phrase($limits[$node][$pairs[0]['value']]) }}@endif</flux:text>
                @endif
            </div>
        @elseif ($provider !== $this::AUTO)
            <flux:select wire:model.live="model" :label="__('Model')" :placeholder="$model === '' ? __('No ready model from this provider') : null" data-test="lane-model">
                @foreach ($groups as $group)
                    @continue($group['provider'] !== $provider)
                    @foreach ($group['models'] as $choice)
                        <flux:select.option :value="$choice['name']" wire:key="model-{{ $choice['name'] }}" :disabled="! $choice['available']">{{ $choice['option'] }}@if (array_key_exists($choice['name'], $limits[$node] ?? [])) — {{ \App\Client\ProviderLimits::phrase($limits[$node][$choice['name']]) }}@endif</flux:select.option>
                    @endforeach
                @endforeach
            </flux:select>
        @endif

        <flux:input wire:model="title" :label="__('Title')" required />
        <flux:textarea wire:model="goal" :label="__('Goal')" rows="8" required />

        @if ($projects !== [])
            <div class="grid gap-4 sm:grid-cols-2">
                <flux:select wire:model.live="projectId" :label="__('Project')" data-test="lane-project">
                    <flux:select.option value="">{{ __('None') }}</flux:select.option>
                    @foreach ($projects as $id => $name)
                        <flux:select.option :value="$id" wire:key="project-{{ $id }}">{{ $name }}</flux:select.option>
                    @endforeach
                </flux:select>
                @if ($projectId !== '')
                    <div class="sm:pt-7">
                        <flux:checkbox wire:model="decisionLog" :label="__('Attach the decision log')" :description="__('The agent gets a frozen copy of the project\'s active decisions.')" data-test="lane-decision-log" />
                    </div>
                @endif
            </div>
        @endif

        <flux:checkbox wire:model.live="scratch" :label="__('Scratch workspace (no repo worktree)')" :description="$scratch ? __('Auto needs a repo to plan against, so a scratch lane runs a model you choose (default: :model).', ['model' => $defaultModel]) : null" />

        @unless ($scratch)
            <div class="grid gap-4 sm:grid-cols-2">
                @if ($repos !== [])
                    <flux:select wire:model="repo" :label="__('Repo')">
                        @foreach ($repos as $name)
                            <flux:select.option :value="$name" wire:key="repo-{{ $name }}">{{ $name }}</flux:select.option>
                        @endforeach
                    </flux:select>
                @else
                    <flux:input wire:model="repo" :label="__('Repo')" :description="__('Name from config/repos.json on the machine')" />
                @endif
                <flux:input wire:model="lane" :label="__('Lane name')" :description="__('Worktree branch lane/NAME')" />
            </div>
            @if ($model !== $this::AUTO)
                <flux:select wire:model="planWith" :label="__('Plan first')" :description="__('A read-only planning job writes a plan for the coding job.')" data-test="lane-plan-with">
                    <flux:select.option value="">{{ __('None') }}</flux:select.option>
                    @foreach ($this::PLANNERS as $planner)
                        <flux:select.option :value="$planner" wire:key="planner-{{ $planner }}">{{ $planner }}</flux:select.option>
                    @endforeach
                </flux:select>
            @endif
        @endunless

        <flux:button type="submit" variant="primary">{{ __('Start task lane') }}</flux:button>
    </form>
</section>
