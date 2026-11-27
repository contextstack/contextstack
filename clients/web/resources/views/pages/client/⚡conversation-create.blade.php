<?php

use App\Client\Bundle;
use App\Client\DirectorClient;
use App\Client\HarnessCatalog;
use App\Client\ModelCatalog;
use App\Client\ProviderLimits;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use Illuminate\Support\Str;
use Illuminate\Validation\Rule;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Attributes\Url;
use Livewire\Component;
use Livewire\WithFileUploads;

/*
 * Start a conversation (CLIENT-API.md §11.1): one that runs tasks (`spec: hosted-lead`) or a plain chat. A chat is the
 * Director's built-in one (`spec: chat`) on the chosen model and machine; behind Advanced options you can instead
 * supply your own spec.yaml, system prompt and files, sent as a bundle like an intent's.
 * The model is picked in three steps: provider, model, then (when more than one can run it) the harness that runs it
 * (HarnessCatalog); the pair's `value` is sent as the model. One that runs tasks offers only `lead: true` models.
 * A Director whose GET models has no `catalog` gets the two-step picker over its flat models (ModelCatalog::groups).
 */
new #[Layout('layouts::client')] #[Title('New conversation')] class extends Component
{
    use WithFileUploads;

    /** `lead` (an agent that can start work: CLIENT-API.md §11.1a) or `chat` (a model that replies). */
    public string $kind = 'lead';

    /** The model a plain chat starts with: the Director's default, kept for switching back from a lead. */
    #[Locked]
    public string $chatDefault = '';

    public string $node = '';

    public string $title = '';

    /** What is sent: the chosen pair's value (a model name). */
    public string $model = '';

    /** The picker's first step: the provider group of the chosen model. */
    public string $provider = '';

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

    public string $message = '';

    public string $spec = '';

    public string $system = '';

    public bool $showAdvanced = false;

    #[Url(as: 'intent')]
    public string $intentId = '';

    #[Url(as: 'project')]
    public string $projectId = '';

    /** @var \Livewire\Features\SupportFileUploads\TemporaryUploadedFile|null */
    public $archive = null;

    #[Locked]
    public string $submitKey = '';

    #[Locked]
    public string $postKey = '';

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

    /**
     * The Director's home machine for conversations that run tasks (`GET lead-node`): they always live there, so the
     * form shows it instead of a machine picker. Null when the Director has no such setting (an older release).
     */
    #[Locked]
    public ?string $homeMachine = null;

    /** @var list<array{name: string, label: string, available: bool, display_name: string, provider: string, billing: string, reason: string|null, lead: bool, model_label: string, provider_label: string, nodes: list<string>, option: string}> */
    #[Locked]
    public array $modelChoices = [];

    #[Locked]
    public bool $canStart = true;

    /** Tasks offered in "Attach to a task": the newest ones. */
    public const INTENT_CHOICES = 100;

    /** @var array<string, string> intent id => title */
    #[Locked]
    public array $intents = [];

    /** @var array<string, string> project id => name */
    #[Locked]
    public array $projects = [];

    public string $problem = '';

    /** @var array{kind: string, id: string}|array{} */
    #[Locked]
    public array $conflict = [];

    public function mount(DirectorClient $director): void
    {
        $this->submitKey = DirectorClient::newKey();
        $this->postKey = DirectorClient::newKey();
        try {
            $nodes = $director->get('nodes');
            $this->nodes = array_values(array_map(fn (array $node): string => (string) $node['name'],
                array_filter((array) ($nodes['nodes'] ?? []), fn ($node): bool => is_array($node) && ($node['revoked_at'] ?? null) === null)));
            $home = $director->find('lead-node');
            $this->homeMachine = is_string($home['node'] ?? null) && $home['node'] !== '' ? $home['node'] : null;
            $this->node = $this->homeMachine !== null && in_array($this->homeMachine, $this->nodes, true) ? $this->homeMachine : ($this->nodes[0] ?? '');
            $this->limits = ProviderLimits::fromNodes($nodes);

            $catalog = $director->get('models');
            $parsed = ModelCatalog::parse($catalog);
            $this->modelChoices = $parsed['choices'];
            $this->canStart = $parsed['canStart'];
            $this->catalog = HarnessCatalog::fromAnswer($catalog);
            if ($this->catalog !== null) {
                $this->model = HarnessCatalog::defaultValue($this->catalog, $this->limitedHere());
            } else {
                $this->model = $parsed['default'] !== '' ? $parsed['default'] : ($this->modelChoices[0]['name'] ?? '');
                $this->model = ProviderLimits::prefer($this->model, $this->readyNames($this->modelChoices), $this->limitedHere());
            }
            $this->chatDefault = $this->model;
            $leadReady = $this->readyLeadValues() !== [];
            $this->kind = $leadReady ? 'lead' : 'chat';
            $this->updatedKind();

            // The newest tasks only, each named in a line: the picker offers recent work, and this list travels
            // with every request the form makes.
            foreach ((array) ($director->get('intents', ['limit' => self::INTENT_CHOICES])['intents'] ?? []) as $intent) {
                if (is_array($intent) && is_string($intent['intent_id'] ?? null)) {
                    $this->intents[$intent['intent_id']] = Str::limit(filled($intent['title'] ?? null) ? (string) $intent['title'] : (string) ($intent['task'] ?? $intent['intent_id']), 120);
                }
            }
            foreach ((array) ($director->get('projects')['projects'] ?? []) as $project) {
                if (is_array($project) && is_string($project['project_id'] ?? null)) {
                    $this->projects[$project['project_id']] = (string) ($project['name'] ?? $project['project_id']);
                }
            }
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();
        }
    }

    /**
     * Models a lead can run on: the catalog's `lead` flag (see ModelCatalog::parse).
     *
     * @return list<array{name: string, label: string, available: bool, display_name: string, provider: string, billing: string, reason: string|null, lead: bool, model_label: string, provider_label: string, nodes: list<string>, option: string}>
     */
    public function leadModelChoices(): array
    {
        return array_values(array_filter($this->modelChoices, fn (array $choice): bool => $choice['lead']));
    }

    /**
     * The models this kind of conversation can start on: lead models for one that runs tasks, all for a chat.
     *
     * @return list<array{name: string, label: string, available: bool, display_name: string, provider: string, billing: string, reason: string|null, lead: bool, model_label: string, provider_label: string, nodes: list<string>, option: string}>
     */
    public function pickableChoices(): array
    {
        return $this->kind === 'lead' ? $this->leadModelChoices() : $this->modelChoices;
    }

    /**
     * Provider groups for the picker.
     *
     * @return list<array{provider: string, label: string, ready: bool, models: list<array{name: string, label: string, available: bool, display_name: string, provider: string, billing: string, reason: string|null, lead: bool, model_label: string, provider_label: string, nodes: list<string>, option: string}>}>
     */
    public function modelGroups(): array
    {
        return ModelCatalog::groups($this->pickableChoices());
    }

    /**
     * The catalog this kind of conversation picks from: lead models only for one that runs tasks.
     *
     * @return array{providers: list<array{id: string, label: string, ready: bool}>, models: list<array{id: string, provider: string, label: string, available: bool, reason: string|null, lead: bool, option: string, pairs: list<array{harness: string, label: string, value: string, billing: string, nodes: list<string>, available: bool, reason: string|null, lead: bool|null, option: string}>}>, default: string}|null
     */
    public function pickableCatalog(): ?array
    {
        if ($this->catalog === null) {
            return null;
        }

        return $this->kind === 'lead' ? HarnessCatalog::forLead($this->catalog) : $this->catalog;
    }

    /**
     * The chosen provider's catalog models, for the second step.
     *
     * @return list<array{id: string, provider: string, label: string, available: bool, reason: string|null, lead: bool, option: string, pairs: list<array{harness: string, label: string, value: string, billing: string, nodes: list<string>, available: bool, reason: string|null, lead: bool|null, option: string}>}>
     */
    public function catalogModels(): array
    {
        $catalog = $this->pickableCatalog();

        return $catalog === null ? [] : HarnessCatalog::modelsOf($catalog, $this->provider);
    }

    /**
     * The harnesses that can run the chosen catalog model, for the third step.
     *
     * @return list<array{harness: string, label: string, value: string, billing: string, nodes: list<string>, available: bool, reason: string|null, lead: bool|null, option: string}>
     */
    public function catalogPairs(): array
    {
        $catalog = $this->pickableCatalog();

        return $catalog === null ? [] : HarnessCatalog::pairsOf($catalog, $this->provider, $this->catalogModel);
    }

    /**
     * Picking a provider selects its first ready model (and that model's harness), preferring one in quota on the
     * machine it would run on.
     */
    public function updatedProvider(): void
    {
        $catalog = $this->pickableCatalog();
        if ($catalog !== null) {
            $this->select(HarnessCatalog::choose($catalog, $this->provider, limited: $this->limitedHere()));
        } else {
            $this->model = ModelCatalog::firstReadyOf($this->provider, $this->pickableChoices(), $this->limitedHere());
        }
    }

    /** Picking a catalog model selects the harness to run it: the catalog default's, else the first ready one. */
    public function updatedCatalogModel(): void
    {
        $catalog = $this->pickableCatalog();
        if ($catalog !== null) {
            $this->select(HarnessCatalog::choose($catalog, $this->provider, $this->catalogModel, limited: $this->limitedHere()));
        }
    }

    public function updatedHarness(): void
    {
        $catalog = $this->pickableCatalog();
        if ($catalog !== null) {
            $this->select(HarnessCatalog::choose($catalog, $this->provider, $this->catalogModel, $this->harness, $this->limitedHere()));
        }
    }

    /** A model set directly (not through the steps) moves the steps to the pair or group that sends it. */
    public function updatedModel(): void
    {
        $catalog = $this->pickableCatalog();
        if ($catalog === null) {
            $this->provider = ModelCatalog::providerOf($this->model, $this->pickableChoices());

            return;
        }
        $found = HarnessCatalog::locate($catalog, $this->model);
        $this->select($found ?? ['provider' => '', 'model' => '', 'harness' => '', 'value' => $this->model]);
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
     * The values a conversation that runs tasks may start on.
     *
     * @return list<string>
     */
    private function readyLeadValues(): array
    {
        return $this->catalog !== null
            ? HarnessCatalog::readyValues(HarnessCatalog::forLead($this->catalog))
            : $this->readyNames($this->leadModelChoices());
    }

    public function updatedKind(): void
    {
        if ($this->kind === 'chat') {
            $this->model = $this->chatDefault;
        }
        if ($this->kind === 'lead') {
            $this->showAdvanced = false;
            $names = $this->readyLeadValues();
            if (! in_array($this->model, $names, true)) {
                $this->model = in_array('claude-code-sonnet', $names, true) ? 'claude-code-sonnet' : ($names[0] ?? '');
            }
            $this->model = ProviderLimits::prefer($this->model, $names, $this->limitedHere());
        }
        $this->updatedModel();
    }

    /**
     * The out-of-quota models on the machine this conversation would run on: the home machine for one that runs
     * tasks, else the machine picked.
     *
     * @return array<string, string|null>
     */
    public function limitedHere(): array
    {
        $node = $this->kind === 'lead' && $this->homeMachine !== null ? $this->homeMachine : $this->node;

        return $this->limits[$node] ?? [];
    }

    /**
     * @param  list<array{name: string, available: bool}>  $choices
     * @return list<string>
     */
    private function readyNames(array $choices): array
    {
        return array_values(array_column(array_filter($choices, fn (array $choice): bool => $choice['available']), 'name'));
    }

    public function submit(DirectorClient $director): void
    {
        if ($this->kind === 'lead') {
            $this->submitLead($director);

            return;
        }

        if (! $this->canStart) {
            return;
        }

        $modelNames = $this->catalog !== null ? HarnessCatalog::readyValues($this->catalog) : $this->readyNames($this->modelChoices);

        $this->validate([
            'node' => ['required', 'string', 'max:128'],
            'model' => ['required', 'string', 'max:128', Rule::in($modelNames ?: ['local-coder'])],
            'title' => ['nullable', 'string', 'max:200'],
            'message' => ['nullable', 'string', 'max:200000'],
            'spec' => ['nullable', 'string', 'max:200000'],
            'system' => ['nullable', 'string', 'max:200000'],
            'intentId' => ['nullable', 'string', 'max:128'],
            'projectId' => ['nullable', 'string', 'max:128'],
            'archive' => ['nullable', 'file', 'max:20480'],
        ]);

        // Your own system prompt or files need your own spec to name them.
        $ownSpec = trim($this->spec) !== '';
        if (! $ownSpec && (trim($this->system) !== '' || $this->archive !== null)) {
            $this->addError('spec', __('Write a spec.yaml to use your own system prompt or files, or leave all three empty for the built-in chat.'));

            return;
        }

        $this->problem = '';
        $this->conflict = [];

        $body = array_filter([
            'node' => $this->node,
            'spec' => $ownSpec ? 'spec.yaml' : 'chat',
            'model' => $ownSpec ? null : $this->model,
            'title' => $this->title !== '' ? $this->title : null,
            'intent_id' => $this->intentId !== '' ? $this->intentId : null,
            'project_id' => $this->projectId !== '' && $this->intentId === '' ? $this->projectId : null,
        ], fn ($value) => $value !== null);

        try {
            if ($ownSpec) {
                try {
                    $bundle = $this->bundle();
                } catch (InvalidArgumentException $invalid) {
                    $this->addError('archive', $invalid->getMessage());

                    return;
                }
                $answer = $director->writeWithBundle('conversations', $body, $bundle, $this->submitKey);
            } else {
                $answer = $director->write('conversations', $body, $this->submitKey);
            }
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Starting it again is safe; it replays the same request.', ['why' => $unavailable->getMessage()]);

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

        $this->openWithFirstMessage($director, (string) $answer->body['conversation']['conversation_id']);
    }

    /**
     * Start a lead: the Director builds its bundle from the installed release, so only the machine, model and title
     * are sent (CLIENT-API.md §11.1a).
     */
    private function submitLead(DirectorClient $director): void
    {
        $leadModels = $this->readyLeadValues();

        $this->validate([
            'node' => ['required', 'string', 'max:128'],
            'model' => ['required', 'string', 'max:128', Rule::in($leadModels ?: ['claude-code-sonnet'])],
            'title' => ['nullable', 'string', 'max:200'],
            'message' => ['nullable', 'string', 'max:200000'],
        ]);

        $this->problem = '';
        $this->conflict = [];

        $body = array_filter([
            'node' => $this->homeMachine ?? $this->node,
            'spec' => 'hosted-lead',
            'model' => $this->model,
            'title' => $this->title !== '' ? $this->title : null,
        ], fn ($value) => $value !== null);

        try {
            $answer = $director->write('conversations', $body, $this->submitKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Starting it again is safe; it replays the same request.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $record = (array) ($refused->body['record'] ?? []);
                $this->conflict = ['kind' => (string) ($record['kind'] ?? ''), 'id' => (string) ($record['id'] ?? '')];
                $this->submitKey = DirectorClient::newKey();
            }
            $this->problem = match (true) {
                $refused->is('lead_bundle_missing') => __('This Director\'s release has no setup for conversations that run tasks. Update ContextStack on the Director\'s machine.'),
                $refused->is('lead_model_unsupported') => $this->unsupportedLeadModel($refused),
                $refused->is('lead_node_mismatch') => __('Conversations that run tasks live on your home machine, and that changed. Reload this page and start again. (:detail)', ['detail' => $refused->explain()]),
                default => $refused->explain(),
            };

            return;
        }

        $this->openWithFirstMessage($director, (string) $answer->body['conversation']['conversation_id']);
    }

    /**
     * The Director lists the models a lead can run on in the refusal's detail (CLIENT-API.md §11.1a).
     */
    private function unsupportedLeadModel(DirectorRefused $refused): string
    {
        $detail = $refused->body['detail'] ?? null;
        $models = is_array($detail) ? array_values(array_filter((array) ($detail['models'] ?? []), 'is_string')) : [];
        if ($models === []) {
            return __('That model cannot run tasks. Choose another.');
        }
        $labels = array_map(fn (string $name): string => ModelCatalog::labelFor($name, $this->modelChoices), $models);

        return __('That model cannot run tasks. These can: :models.', ['models' => implode(', ', $labels)]);
    }

    /**
     * Send the first message, if any, then open the new conversation.
     */
    private function openWithFirstMessage(DirectorClient $director, string $conversationId): void
    {
        $firstMessage = trim($this->message);

        if ($firstMessage !== '') {
            try {
                $director->write('conversations/'.rawurlencode($conversationId).'/turns', ['message' => $firstMessage], $this->postKey);
            } catch (DirectorUnavailable|DirectorRefused $failed) {
                // This page is left behind: the conversation's page shows this (its mount reads the flash).
                session()->flash('conversation-problem', __('The conversation started, but your first message did not reach it: :why. Send it again below.', [
                    'why' => $failed instanceof DirectorRefused ? $failed->explain() : $failed->getMessage(),
                ]));
                $this->redirectRoute('conversations.show', ['conversation' => $conversationId], navigate: true);

                return;
            }
        }

        $this->submitKey = DirectorClient::newKey();
        $this->postKey = DirectorClient::newKey();
        $this->redirectRoute('conversations.show', ['conversation' => $conversationId], navigate: true);
    }

    private function bundle(): Bundle
    {
        $bundle = new Bundle;
        if ($this->archive !== null) {
            $bundle->addZip($this->archive->getRealPath());
        }
        $bundle->add('spec.yaml', $this->spec);
        if (trim($this->system) !== '') {
            $bundle->add('prompts/system.md', $this->system);
        }

        return $bundle;
    }
};
?>

<section class="mx-auto flex w-full max-w-3xl flex-col gap-5" data-test="conversation-create-page">
    <div>
        <flux:heading size="lg" level="1" class="text-lg!">{{ __('New conversation') }}</flux:heading>
        <flux:text class="mt-0.5">{{ __('Say what you want. A conversation that runs tasks plans the work, starts task lanes on your machines, and asks you when it needs a decision.') }}</flux:text>
    </div>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
            @if (($conflict['kind'] ?? '') === 'conversation' && $conflict['id'] !== '')
                <x-slot name="actions">
                    <flux:button size="sm" :href="route('conversations.show', $conflict['id'])" wire:navigate>{{ __('Open the recorded conversation') }}</flux:button>
                </x-slot>
            @endif
        </flux:callout>
    @endif

    @capable('start_work')
    <form wire:submit="submit" class="space-y-6">
        <flux:radio.group wire:model.live="kind" variant="segmented" data-test="kind">
            <flux:radio value="lead" :label="__('Runs tasks')" />
            <flux:radio value="chat" :label="__('Chat only')" />
        </flux:radio.group>

        @if ($kind === 'lead' && $homeMachine !== null)
            <flux:text class="text-sm text-zinc-600 dark:text-zinc-400" data-test="home-machine">
                {{ __('Runs on') }} <span class="font-mono text-zinc-900 dark:text-zinc-100">{{ $homeMachine }}</span>, {{ __('your home machine for conversations that run tasks. The task lanes it starts can run on any machine.') }}
                <flux:link :href="route('connect')" wire:navigate>{{ __('Change it in Settings') }}</flux:link>
            </flux:text>
        @elseif (count($nodes) > 1)
            <flux:select wire:model.live="node" :label="__('Machine')">
                @foreach ($nodes as $name)
                    <flux:select.option :value="$name" wire:key="node-{{ $name }}">{{ $name }}</flux:select.option>
                @endforeach
            </flux:select>
        @elseif (count($nodes) === 1)
            <flux:text class="text-sm text-zinc-600 dark:text-zinc-400">{{ __('Machine') }} <span class="font-mono">{{ $nodes[0] }}</span></flux:text>
        @endif

        @if (! $canStart)
            <flux:callout variant="warning" icon="exclamation-circle" data-test="models-unready">
                <flux:callout.text>{{ __(\App\Client\ModelCatalog::NoReadyHint) }}</flux:callout.text>
            </flux:callout>
        @endif

        @php($limitedHere = $this->limitedHere())
        @php($pickable = $this->pickableCatalog())
        @if ($pickable !== null)
            @php($pairs = $this->catalogPairs())
            <div class="grid gap-4 sm:grid-cols-2">
                <flux:select wire:model.live="provider" :label="__('Provider')" data-test="provider-select">
                    @foreach ($pickable['providers'] as $catalogProvider)
                        <flux:select.option :value="$catalogProvider['id']" wire:key="provider-{{ $kind }}-{{ $catalogProvider['id'] }}">{{ $catalogProvider['label'] }}@unless ($catalogProvider['ready']) — {{ __('not ready') }}@endunless</flux:select.option>
                    @endforeach
                </flux:select>
                <flux:select wire:model.live="catalogModel" :label="__('Model')" :placeholder="$catalogModel === '' ? __('No model from this provider') : null" data-test="model-select">
                    @foreach ($this->catalogModels() as $catalogChoice)
                        <flux:select.option :value="$catalogChoice['id']" wire:key="model-{{ $kind }}-{{ $provider }}-{{ $catalogChoice['id'] }}" :disabled="! $catalogChoice['available']">{{ $catalogChoice['option'] }}</flux:select.option>
                    @endforeach
                </flux:select>
            </div>
            @if (count($pairs) > 1)
                <flux:select wire:model.live="harness" :label="__('Harness')" :placeholder="$harness === '' ? __('No ready harness for this model') : null" data-test="harness-select">
                    @foreach ($pairs as $pair)
                        <flux:select.option :value="$pair['harness']" wire:key="harness-{{ $kind }}-{{ $catalogModel }}-{{ $pair['harness'] }}" :disabled="! $pair['available']">{{ $pair['option'] }}@if (array_key_exists($pair['value'], $limitedHere)) — {{ \App\Client\ProviderLimits::phrase($limitedHere[$pair['value']]) }}@endif</flux:select.option>
                    @endforeach
                </flux:select>
            @elseif (count($pairs) === 1)
                <flux:text class="text-sm text-zinc-600 dark:text-zinc-400" data-test="harness-text">{{ \App\Client\HarnessCatalog::runsIn($pairs[0]) }}@if (array_key_exists($pairs[0]['value'], $limitedHere)) — {{ \App\Client\ProviderLimits::phrase($limitedHere[$pairs[0]['value']]) }}@endif</flux:text>
            @endif
        @else
            @php($groups = $this->modelGroups())
            <div class="grid gap-4 sm:grid-cols-2">
                <flux:select wire:model.live="provider" :label="__('Provider')" data-test="provider-select">
                    @foreach ($groups as $group)
                        <flux:select.option :value="$group['provider']" wire:key="provider-{{ $kind }}-{{ $group['provider'] }}">{{ $group['label'] }}@unless ($group['ready']) — {{ __('not ready') }}@endunless</flux:select.option>
                    @endforeach
                </flux:select>
                <flux:select wire:model.live="model" :label="__('Model')" :placeholder="$model === '' ? __('No ready model from this provider') : null" data-test="model-select">
                    @foreach ($groups as $group)
                        @continue($group['provider'] !== $provider)
                        @foreach ($group['models'] as $choice)
                            <flux:select.option :value="$choice['name']" wire:key="model-{{ $kind }}-{{ $choice['name'] }}" :disabled="! $choice['available']">
                                {{ $choice['option'] }}@if (array_key_exists($choice['name'], $limitedHere)) — {{ \App\Client\ProviderLimits::phrase($limitedHere[$choice['name']]) }}@endif
                            </flux:select.option>
                        @endforeach
                    @endforeach
                </flux:select>
            </div>
        @endif

        <flux:input wire:model="title" :label="__('Title')" :placeholder="__('optional')" />

        {{-- The same Markdown editor as a conversation's message box: Enter starts, Shift+Enter is a new line. --}}
        <flux:field>
            <flux:label>{{ __('First message') }}</flux:label>
            <div data-test="first-message" data-cs-composer x-data="composer({ form: true })" x-on:click="focusBox($event)"
                 class="rounded-xl border border-black/10 bg-white px-3 py-2 shadow-xs focus-within:border-black/25 dark:border-white/10 dark:bg-white/[.04] dark:focus-within:border-white/25">
                <div wire:ignore>
                    <ui-editor x-ref="editor" class="block min-h-[5.5rem] w-full" data-flux-editor data-flux-control aria-label="{{ __('First message') }}"
                               placeholder="{{ __('What should we work on?') }}" x-on:keydown.capture="keydown($event)">
                        <ui-editor-content></ui-editor-content>
                    </ui-editor>
                </div>
            </div>
            <flux:error name="message" />
        </flux:field>

        @if ($kind === 'chat')
        <div>
            <flux:button type="button" variant="ghost" wire:click="$toggle('showAdvanced')" data-test="advanced-toggle">
                {{ $showAdvanced ? __('Hide advanced options') : __('Advanced options') }}
            </flux:button>
        </div>

        @if ($showAdvanced)
            <div class="space-y-6 rounded-lg border border-zinc-200 p-4 dark:border-zinc-700" data-test="advanced-fields">
                <div class="grid gap-4 sm:grid-cols-2">
                    <flux:select wire:model.live="intentId" :label="__('Attach to a task')">
                        <flux:select.option value="">{{ __('none: it stands alone') }}</flux:select.option>
                        @foreach ($intents as $id => $label)
                            <flux:select.option :value="$id" wire:key="intent-{{ $id }}">{{ $label }}</flux:select.option>
                        @endforeach
                    </flux:select>
                    <flux:select wire:model="projectId" :label="__('Project')" :disabled="$intentId !== ''"
                                 :description="$intentId !== '' ? __('An attached conversation takes its task\'s project.') : null">
                        <flux:select.option value="">{{ __('none') }}</flux:select.option>
                        @foreach ($projects as $id => $name)
                            <flux:select.option :value="$id" wire:key="project-{{ $id }}">{{ $name }}</flux:select.option>
                        @endforeach
                    </flux:select>
                </div>

                <flux:text>{{ __('Leave these empty for the built-in chat on the model above. Your own spec replaces it, and then names its model itself.') }}</flux:text>
                <flux:textarea wire:model="spec" :label="__('Your own spec (spec.yaml)')" rows="10" class="font-mono"
                               :description="__('It must list conversation/context.md in context and tasks/turn in tasks.')" data-test="spec-yaml" />
                <flux:textarea wire:model="system" :label="__('System prompt (prompts/system.md)')" rows="3" />

                <flux:input type="file" wire:model="archive" :label="__('More bundle files (a .zip, optional)')" :description="__('Added under their own paths; the fields above take precedence.')" />
            </div>
        @endif
        @endif

        <flux:button type="submit" variant="primary" :disabled="! $canStart" data-test="start-conversation">{{ __('Start') }}</flux:button>
    </form>
    @else
        <flux:callout icon="information-circle" data-test="cannot-start">
            <flux:callout.text>{{ __('Your role can read work here but not start new conversations. An owner, admin or member can.') }}</flux:callout.text>
        </flux:callout>
    @endcapable

    @assets
        <flux:editor.scripts />
        <flux:editor.styles />
    @endassets
</section>
