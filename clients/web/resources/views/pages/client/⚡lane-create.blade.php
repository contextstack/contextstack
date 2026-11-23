<?php

use App\Client\DirectorClient;
use App\Client\ProviderLimits;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Attributes\Url;
use Illuminate\Support\Str;
use Livewire\Component;

/*
 * Start a coding lane: the Director's built-in coding lane (`POST intents` with `lane`, CLIENT-API "Built-in coding
 * lane intents"), the same definition `cs mcp start_task` uses. The Director writes the spec, system prompt, goal and
 * check; the app sends the title, the goal, the model, and the repo and lane name (or neither, for scratch work).
 * A lane may belong to a project, and then carry the project's active decision log as context
 * (`lane.context: ["decision_log"]` with the intent's `project_id`): the Director freezes a copy into the job.
 */
new #[Layout('layouts::client')] #[Title('New task lane')] class extends Component
{
    public string $node = '';

    public string $title = '';

    public string $goal = '';

    public string $model = '';

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

    /** @var list<array{name: string, label: string, available: bool}> */
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
            foreach ((array) ($catalog['models'] ?? []) as $row) {
                if (! is_array($row) || ! is_string($row['name'] ?? null)) {
                    continue;
                }
                $this->modelChoices[] = [
                    'name' => $row['name'],
                    'label' => (string) ($row['label'] ?? $row['name']),
                    'available' => (bool) ($row['available'] ?? true),
                ];
            }
            $default = is_string($catalog['default_model'] ?? null) ? $catalog['default_model'] : '';
            $this->model = $default !== '' ? $default : ($this->modelChoices[0]['name'] ?? 'local-coder');
            $ready = array_values(array_column(array_filter($this->modelChoices, fn (array $choice): bool => $choice['available']), 'name'));
            $this->model = ProviderLimits::prefer($this->model, $ready, $this->limits[$this->node] ?? []);

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

    public function submit(DirectorClient $director): void
    {
        $rules = [
            'node' => ['required', 'string', 'max:128'],
            'title' => ['required', 'string', 'max:200'],
            'goal' => ['required', 'string', 'max:200000'],
            'model' => ['required', 'string', 'max:128', 'regex:/^[a-z0-9][a-z0-9._-]{0,62}$/'],
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
            <flux:select wire:model="model" :label="__('Model')">
                @foreach ($modelChoices as $choice)
                    <flux:select.option :value="$choice['name']" wire:key="model-{{ $choice['name'] }}" :disabled="! $choice['available']">{{ $choice['label'] }}@if (array_key_exists($choice['name'], $limits[$node] ?? [])) — {{ \App\Client\ProviderLimits::phrase($limits[$node][$choice['name']]) }}@endif</flux:select.option>
                @endforeach
            </flux:select>
        </div>

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

        <flux:checkbox wire:model.live="scratch" :label="__('Scratch workspace (no repo worktree)')" />

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
        @endunless

        <flux:button type="submit" variant="primary">{{ __('Start task lane') }}</flux:button>
    </form>
</section>
