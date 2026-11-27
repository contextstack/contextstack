<?php

use App\Client\Bundle;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Attributes\Url;
use Livewire\Component;
use Livewire\WithFileUploads;

/*
 * Submit an intent naming its spec, task and node (CLIENT-API.md §4.1). The client supplies the bundle: the spec,
 * the task's goal and check (when none is given, the Director adds its "no check" that always passes and marks the
 * intent check_added: none), a system prompt, and optionally a zip of further files. Every file is uploaded as an
 * object, then listed in the intent.
 *
 * One Idempotency-Key per submission: kept while the answer may have been lost (a retry replays it), replaced once
 * the intent is recorded or the key turns out to name a different request.
 */
new #[Layout('layouts::client')] #[Title('New task')] class extends Component
{
    use WithFileUploads;

    public string $node = '';

    public string $title = '';

    public string $taskName = 'hello';

    public string $spec = "id: hello\nmodel: local-coder\nsystem: prompts/system.md\ncontext: []\ntools: []\nmcp: []\nbudget: {turns: 4, tool_calls: 0, usd: 0, seconds: 300}\ntasks: [tasks/hello]\ntrials: 1\n";

    public string $system = "You are a careful assistant.\n";

    public string $goal = '';

    public string $check = '';

    public int $trials = 1;

    public string $grant = '';

    #[Url(as: 'project')]
    public string $projectId = '';

    /** @var array<string, string> project id => name */
    #[Locked]
    public array $projects = [];

    /** @var \Livewire\Features\SupportFileUploads\TemporaryUploadedFile|null */
    public $archive = null;

    #[Locked]
    public string $submitKey = '';

    /** @var list<string> */
    #[Locked]
    public array $nodes = [];

    public string $problem = '';

    /** @var array{kind: string, id: string}|array{} */
    #[Locked]
    public array $conflict = [];

    public function mount(): void
    {
        $this->submitKey = DirectorClient::newKey();
        try {
            $answer = app(DirectorClient::class)->get('nodes');
            $this->nodes = array_values(array_map(fn (array $node): string => (string) $node['name'],
                array_filter((array) ($answer['nodes'] ?? []), fn ($node): bool => is_array($node) && ($node['revoked_at'] ?? null) === null)));
            $this->node = $this->nodes[0] ?? '';
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();
        }

        // Projects only offer a choice; without them an intent is still submitted as before.
        try {
            foreach ((array) (app(DirectorClient::class)->get('projects')['projects'] ?? []) as $project) {
                if (is_array($project) && is_string($project['project_id'] ?? null)) {
                    $this->projects[$project['project_id']] = (string) ($project['name'] ?? $project['project_id']);
                }
            }
        } catch (DirectorRefused|DirectorUnavailable) {
            $this->projects = [];
        }
    }

    public function submit(DirectorClient $director): void
    {
        $this->validate([
            'node' => ['required', 'string', 'max:128'],
            'title' => ['nullable', 'string', 'max:200'],
            'taskName' => ['required', 'string', 'max:100', 'regex:/^[A-Za-z0-9][A-Za-z0-9._-]*$/'],
            'spec' => ['required', 'string', 'max:200000'],
            'system' => ['nullable', 'string', 'max:200000'],
            'goal' => ['required', 'string', 'max:200000'],
            'check' => ['nullable', 'string', 'max:200000'],
            'trials' => ['required', 'integer', 'min:1', 'max:100'],
            'grant' => ['nullable', 'string', 'max:128', 'regex:/^[A-Za-z0-9][A-Za-z0-9._-]*$/'],
            'archive' => ['nullable', 'file', 'max:20480'],
            'projectId' => ['nullable', 'string', 'max:128'],
        ]);

        $this->problem = '';
        $this->conflict = [];
        $task = 'tasks/'.$this->taskName;

        try {
            $bundle = $this->bundle($task);
        } catch (InvalidArgumentException $invalid) {
            $this->addError('archive', $invalid->getMessage());

            return;
        }

        if ($this->grant !== '' && ! $bundle->has('grants/'.$this->grant.'.yaml')) {
            $this->addError('grant', __('The bundle needs grants/:grant.yaml for this grant (add it in the zip).', ['grant' => $this->grant]));

            return;
        }

        $intent = array_filter([
            'node' => $this->node,
            'spec' => 'spec.yaml',
            'task' => $task,
            'trials' => $this->trials,
            'grant' => $this->grant !== '' ? $this->grant : null,
            'title' => $this->title !== '' ? $this->title : null,
            'project_id' => $this->projectId !== '' ? $this->projectId : null,
        ], fn ($value) => $value !== null);

        try {
            $answer = $director->submitIntent($intent, $bundle, $this->submitKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Submitting again is safe; it replays the same request.', ['why' => $unavailable->getMessage()]);

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
        $this->redirectRoute('intents.show', ['intent' => $answer->body['intent']['intent_id']], navigate: true);
    }

    private function bundle(string $task): Bundle
    {
        $bundle = new Bundle;
        if ($this->archive !== null) {
            $bundle->addZip($this->archive->getRealPath());
        }
        $bundle->add('spec.yaml', $this->spec);
        $bundle->add($task.'/goal.md', $this->goal);
        // Without a check of its own, the Director adds its "no check" (every run passes).
        if (trim($this->check) !== '') {
            $bundle->add($task.'/check', $this->check, Bundle::Executable);
        }
        if (trim($this->system) !== '') {
            $bundle->add('prompts/system.md', $this->system);
        }

        return $bundle;
    }
};
?>

<section class="mx-auto flex w-full max-w-3xl flex-col gap-5" data-test="intent-create-page">
    <div>
        <flux:heading size="lg" level="1" class="text-lg!">{{ __('New task') }}</flux:heading>
        <flux:text class="mt-0.5">{{ __('Name the spec, the task and the machine. The Director makes one job exactly as told.') }}</flux:text>
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

    @capable('start_work')
    <form wire:submit="submit" class="space-y-6">
        <div class="grid gap-4 sm:grid-cols-2">
            <flux:select wire:model="node" :label="__('Machine')">
                @foreach ($nodes as $name)
                    <flux:select.option :value="$name" wire:key="node-{{ $name }}">{{ $name }}</flux:select.option>
                @endforeach
            </flux:select>
            <flux:input wire:model="title" :label="__('Title')" :placeholder="__('optional')" />
        </div>

        <flux:select wire:model="projectId" :label="__('Project')" :description="__('Under a project budget the job is given what its spec states (budget.usd) or what is left, whichever is less; with nothing left it waits.')">
            <flux:select.option value="">{{ __('none') }}</flux:select.option>
            @foreach ($projects as $id => $name)
                <flux:select.option :value="$id" wire:key="project-{{ $id }}">{{ $name }}</flux:select.option>
            @endforeach
        </flux:select>

        <flux:textarea wire:model="spec" :label="__('Spec (spec.yaml)')" rows="10" class="font-mono" />
        <flux:textarea wire:model="system" :label="__('System prompt (prompts/system.md)')" rows="3" />

        <div class="grid gap-4 sm:grid-cols-3">
            <flux:input wire:model="taskName" :label="__('Task')" :description="__('tasks/NAME')" />
            <flux:input type="number" wire:model="trials" :label="__('Trials')" min="1" max="100" />
            <flux:input wire:model="grant" :label="__('Grant')" :placeholder="__('optional')" />
        </div>

        <flux:textarea wire:model="goal" :label="__('Goal (tasks/NAME/goal.md)')" rows="5" />
        <flux:textarea wire:model="check" :label="__('Check script (tasks/NAME/check; empty means no check: every run passes)')" rows="3" class="font-mono" />

        <flux:input type="file" wire:model="archive" :label="__('More bundle files (a .zip, optional)')" :description="__('Added under their own paths; the fields above take precedence.')" />

        <flux:button type="submit" variant="primary">{{ __('Submit') }}</flux:button>
    </form>
    @else
        <flux:callout icon="information-circle" data-test="cannot-start">
            <flux:callout.text>{{ __('Your role can read work here but not start new tasks. An owner, admin or member can.') }}</flux:callout.text>
        </flux:callout>
    @endcapable
</section>
