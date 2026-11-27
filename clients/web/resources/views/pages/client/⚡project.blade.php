<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Livewire\DirectorPage;
use App\Livewire\Forms\BudgetForm;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * One project (CLIENT-API.md §12): its budget as the Director shows it ("no budget set", "active", "reserved",
 * "budget used up" each distinct), set or cleared; the jobs its budget holds back, each with its reason; and the
 * intents and conversations that name it. Work is attached to a project when it is created (it names the
 * project_id), so this page starts new work in it.
 */
new #[Layout('layouts::client')] #[Title('Project')] class extends DirectorPage
{
    public BudgetForm $budget;

    #[Locked]
    public string $projectId = '';

    /** @var array<string, mixed> */
    #[Locked]
    public array $usage = [];

    /**
     * The project's heartbeat (`GET heartbeat`): `{interval_seconds, conversation_id}`, both null when off; null when
     * the Director has no heartbeat (an older release answers 404) and the card stays hidden.
     *
     * @var array<string, mixed>|null
     */
    #[Locked]
    public ?array $heartbeat = null;

    public int|string $heartbeatMinutes = 60;

    public string $heartbeatConversation = '';

    public string $heartbeatProblem = '';

    public function mount(string $project): void
    {
        $this->projectId = $project;
        $this->refetch();
        abort_if($this->refusedStatus === 404, 404);
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get('projects/'.rawurlencode($this->projectId)));
        if ($answer === null) {
            return;
        }

        $this->records = [];
        $this->hold('project', $this->projectId, (array) $answer['project']);

        $intents = $this->attempt(fn () => $this->director()->get('intents', ['project' => $this->projectId]));
        foreach ((array) ($intents['intents'] ?? []) as $intent) {
            if (($intent['project_id'] ?? null) === $this->projectId) {
                $this->hold('intent', (string) $intent['intent_id'], $intent);
            }
        }
        $conversations = $this->attempt(fn () => $this->director()->get('conversations', ['project' => $this->projectId]));
        foreach ((array) ($conversations['conversations'] ?? []) as $conversation) {
            if (($conversation['project_id'] ?? null) === $this->projectId) {
                $this->hold('conversation', (string) $conversation['conversation_id'], $conversation);
            }
        }
        $this->loadHeartbeat();
        $this->afterChanges();
    }

    protected function follows(array $change): bool
    {
        $record = (array) ($change['record'] ?? []);

        return match ($change['kind'] ?? null) {
            'project' => ($change['id'] ?? null) === $this->projectId,
            'intent', 'conversation' => ($record['project_id'] ?? null) === $this->projectId,
            'job' => ($record['budget']['project_id'] ?? null) === $this->projectId,
            default => false,
        };
    }

    /**
     * Fetch each job the budget holds back that is not held yet, and the project's usage.
     */
    protected function afterChanges(): void
    {
        foreach ((array) ($this->project()['budget']['waiting_job_ids'] ?? []) as $jobId) {
            if (is_string($jobId) && $this->held('job', $jobId) === null) {
                $job = $this->attempt(fn () => $this->director()->get('jobs/'.rawurlencode($jobId)));
                if (is_array($job)) {
                    $this->hold('job', $jobId, $job);
                }
            }
        }

        $usage = $this->attempt(fn () => $this->director()->get('usage', ['by' => 'project', 'project' => $this->projectId, 'period' => 'all']));
        if (is_array($usage)) {
            $this->usage = self::withoutEstimates($usage);
        }
    }

    public function setBudget(): void
    {
        $this->take($this->attempt(fn () => $this->budget->put($this->director(), 'projects/'.rawurlencode($this->projectId).'/budget')));
    }

    public function clearBudget(): void
    {
        $this->take($this->attempt(fn () => $this->budget->clear($this->director(), 'projects/'.rawurlencode($this->projectId).'/budget')));
    }

    public function saveHeartbeat(): void
    {
        $this->heartbeatProblem = '';
        $leads = array_keys($this->leadConversations());
        $this->validate([
            'heartbeatMinutes' => ['required', 'integer', 'min:1', 'max:10080'],
            'heartbeatConversation' => ['required', 'string', 'in:'.implode(',', $leads ?: [''])],
        ], ['heartbeatConversation.required' => __('Choose the hosted lead that takes the next item.')]);

        $this->writeHeartbeat(['interval_seconds' => (int) $this->heartbeatMinutes * 60, 'conversation_id' => $this->heartbeatConversation]);
    }

    public function stopHeartbeat(): void
    {
        $this->heartbeatProblem = '';
        $this->writeHeartbeat(['interval_seconds' => null]);
    }

    /**
     * The project's hosted lead conversations that are not archived: the heartbeat's possible targets.
     *
     * @return array<string, string> conversation id => title
     */
    public function leadConversations(): array
    {
        $leads = [];
        foreach ($this->heldOf('conversation') as $conversation) {
            if (! empty($conversation['lead']) && ($conversation['archived_at'] ?? null) === null) {
                $id = (string) $conversation['conversation_id'];
                $leads[$id] = filled($conversation['title'] ?? null) ? (string) $conversation['title'] : $id;
            }
        }

        return $leads;
    }

    /**
     * @param  array<string, mixed>  $body
     */
    private function writeHeartbeat(array $body): void
    {
        try {
            $answer = $this->director()->put('projects/'.rawurlencode($this->projectId).'/heartbeat', $body, DirectorClient::newKey());
        } catch (DirectorRefused $refused) {
            $this->heartbeatProblem = $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->heartbeatProblem = $unavailable->getMessage();

            return;
        }

        $this->heartbeat = $answer;
        $this->fillHeartbeatForm();
    }

    private function loadHeartbeat(): void
    {
        try {
            $this->heartbeat = $this->director()->get('projects/'.rawurlencode($this->projectId).'/heartbeat');
        } catch (DirectorRefused|DirectorUnavailable) {
            $this->heartbeat = null;

            return;
        }
        $this->fillHeartbeatForm();
    }

    private function fillHeartbeatForm(): void
    {
        $seconds = $this->heartbeat['interval_seconds'] ?? null;
        $this->heartbeatMinutes = is_int($seconds) && $seconds > 0 ? max(1, intdiv($seconds, 60)) : 60;
        $this->heartbeatConversation = is_string($this->heartbeat['conversation_id'] ?? null) ? $this->heartbeat['conversation_id'] : (string) (array_key_first($this->leadConversations()) ?? '');
    }

    /**
     * @return array<string, mixed>|null
     */
    public function project(): ?array
    {
        return $this->held('project', $this->projectId);
    }

    /**
     * The jobs the budget holds back, as listed in its waiting_job_ids.
     *
     * @return list<array<string, mixed>>
     */
    public function waitingJobs(): array
    {
        $jobs = [];
        foreach ((array) ($this->project()['budget']['waiting_job_ids'] ?? []) as $jobId) {
            $job = is_string($jobId) ? $this->held('job', $jobId) : null;
            if ($job !== null && ($job['state'] ?? null) === 'waiting') {
                $jobs[] = $job;
            }
        }

        return $jobs;
    }

    /**
     * @param  array<string, mixed>|null  $answer
     */
    private function take(?array $answer): void
    {
        if (is_array($answer['project'] ?? null)) {
            $this->hold('project', $this->projectId, $answer['project']);
            $this->afterChanges();
        }
    }
};
?>

<section class="mx-auto flex w-full max-w-6xl flex-col gap-6" data-test="project-page"
         x-data="directorStream(@js(route('stream')))">
    @php($project = $this->project())

    <x-client.page-header :back="route('projects.index')" :back-label="__('Projects')">
        <span data-test="project-name">{{ $project['name'] ?? __('Project') }}</span>
        <x-slot:actions>
            @if ($project)
                <flux:button size="sm" variant="ghost" icon="queue-list" :href="route('backlog.index', $project['project_id'])" wire:navigate data-test="project-backlog">{{ __('Backlog') }}</flux:button>
                <flux:button size="sm" variant="ghost" icon="scale" :href="route('decisions.index', $project['project_id'])" wire:navigate data-test="project-decision-log">{{ __('Decision log') }}</flux:button>
                <flux:button size="sm" variant="ghost" icon="code-bracket-square" :href="route('lanes.create', ['project' => $project['project_id']])" wire:navigate data-test="project-new-lane">{{ __('New task lane') }}</flux:button>
                <flux:button size="sm" variant="ghost" icon="plus" :href="route('intents.create', ['project' => $project['project_id']])" wire:navigate>{{ __('New task in this project') }}</flux:button>
                <flux:button size="sm" variant="ghost" icon="chat-bubble-left-right" :href="route('conversations.create', ['project' => $project['project_id']])" wire:navigate>{{ __('New conversation in this project') }}</flux:button>
            @endif
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($project)
        <div class="grid gap-x-10 gap-y-6 lg:grid-cols-[minmax(0,1fr)_19rem]">
            <div class="min-w-0 space-y-8">
                <x-client.section :heading="__('Tasks')" data-test="project-intents">
                    <ul class="divide-y divide-black/5 dark:divide-white/5">
                        @forelse ($this->heldOf('intent') as $intent)
                            <li class="flex items-center gap-3 py-2" wire:key="intent-{{ $intent['intent_id'] }}">
                                <flux:link :href="route('intents.show', $intent['intent_id'])" wire:navigate class="min-w-0 flex-1 truncate text-sm">{{ filled($intent['title'] ?? null) ? $intent['title'] : $intent['task'] }}</flux:link>
                                <span class="shrink-0 text-xs text-zinc-500">{{ $intent['node'] }} · <x-client.time :at="$intent['created_at']" /></span>
                            </li>
                        @empty
                            <li class="py-2 text-sm text-zinc-500">{{ __('None yet.') }}</li>
                        @endforelse
                    </ul>
                </x-client.section>

                <x-client.section :heading="__('Conversations')" data-test="project-conversations">
                    <ul class="divide-y divide-black/5 dark:divide-white/5">
                        @forelse ($this->heldOf('conversation') as $conversation)
                            <li class="flex items-center gap-3 py-2" wire:key="conversation-{{ $conversation['conversation_id'] }}">
                                <flux:link :href="route('conversations.show', $conversation['conversation_id'])" wire:navigate class="min-w-0 flex-1 truncate text-sm">{{ filled($conversation['title'] ?? null) ? $conversation['title'] : $conversation['conversation_id'] }}</flux:link>
                                <x-client.conversation-state :conversation="$conversation" />
                            </li>
                        @empty
                            <li class="py-2 text-sm text-zinc-500">{{ __('None yet.') }}</li>
                        @endforelse
                    </ul>
                </x-client.section>
            </div>

            <aside class="space-y-6 lg:border-s lg:border-black/[.06] lg:ps-6 lg:dark:border-white/[.06]">
                <x-client.facts>
                    <x-client.fact :label="__('ID')" mono title="{{ $project['project_id'] }}">{{ $project['project_id'] }}</x-client.fact>
                </x-client.facts>

                <x-client.section :heading="__('Budget')" data-test="project-budget">
                    <p class="text-xs leading-5 text-zinc-500">{{ __('Dollars of actual pay-as-you-go spending, on top of the gateway\'s source budgets. A job is given what its spec states or what is left, whichever is less; with nothing left it waits.') }}</p>
                    <x-client.budget :budget="$project['budget'] ?? null" />
                    <x-client.waiting-jobs :jobs="$this->waitingJobs()" />
                    @capable('manage_director_budgets')
                    <form wire:submit="setBudget" class="flex flex-wrap items-end gap-2">
                        <flux:input size="sm" wire:model="budget.usd" :label="__('Dollars')" type="number" step="0.01" min="0" class="w-24" />
                        <flux:select size="sm" wire:model="budget.period" :label="__('Per')" class="w-28">
                            <flux:select.option value="total">{{ __('total') }}</flux:select.option>
                            <flux:select.option value="day">{{ __('day') }}</flux:select.option>
                            <flux:select.option value="month">{{ __('month') }}</flux:select.option>
                        </flux:select>
                        <flux:button size="sm" type="submit">{{ __('Set budget') }}</flux:button>
                        @if (($project['budget']['state'] ?? 'none') !== 'none')
                            <flux:button size="sm" type="button" variant="ghost" wire:click="clearBudget" data-test="clear-budget">{{ __('Clear budget') }}</flux:button>
                        @endif
                    </form>
                    @endcapable
                </x-client.section>

                @if ($heartbeat !== null)
                    @php($beating = is_int($heartbeat['interval_seconds'] ?? null))
                    <x-client.section :heading="__('Heartbeat')" data-test="project-heartbeat">
                        <p class="text-xs leading-5 text-zinc-500">{{ __('On a schedule, a hosted lead takes the next unblocked backlog item. It waits one interval first, never overlaps itself, and pauses while an item marked stop waits on you.') }}</p>
                        <p class="text-sm" data-test="heartbeat-state">
                            @if ($beating)
                                {{ __('On: every :n min, in :conversation.', ['n' => intdiv($heartbeat['interval_seconds'], 60), 'conversation' => $this->leadConversations()[$heartbeat['conversation_id']] ?? $heartbeat['conversation_id']]) }}
                            @else
                                {{ __('Off.') }}
                            @endif
                        </p>
                        @if ($this->leadConversations() === [])
                            <p class="text-sm text-zinc-500">{{ __('Start a hosted lead conversation in this project to give the heartbeat somewhere to run.') }}</p>
                        @else
                            @capable('edit_projects')
                            <form wire:submit="saveHeartbeat" class="flex flex-wrap items-end gap-2">
                                <flux:input size="sm" wire:model="heartbeatMinutes" :label="__('Every (minutes)')" type="number" min="1" class="w-28" data-test="heartbeat-minutes" />
                                <flux:select size="sm" wire:model="heartbeatConversation" :label="__('Lead')" class="min-w-0 flex-1" data-test="heartbeat-conversation">
                                    @foreach ($this->leadConversations() as $id => $title)
                                        <flux:select.option :value="$id" wire:key="lead-{{ $id }}">{{ $title }}</flux:select.option>
                                    @endforeach
                                </flux:select>
                                <flux:button size="sm" type="submit" data-test="save-heartbeat">{{ $beating ? __('Update') : __('Turn on') }}</flux:button>
                                @if ($beating)
                                    <flux:button size="sm" type="button" variant="ghost" wire:click="stopHeartbeat" data-test="stop-heartbeat">{{ __('Turn off') }}</flux:button>
                                @endif
                            </form>
                            @endcapable
                        @endif
                        @error('heartbeatMinutes') <flux:text class="text-sm text-red-600">{{ $message }}</flux:text> @enderror
                        @error('heartbeatConversation') <flux:text class="text-sm text-red-600">{{ $message }}</flux:text> @enderror
                        @if ($heartbeatProblem !== '')
                            <flux:text class="text-sm text-red-600" data-test="heartbeat-problem">{{ $heartbeatProblem }}</flux:text>
                        @endif
                    </x-client.section>
                @endif

                @if (is_array($usage['total'] ?? null))
                    <x-client.section :heading="__('Usage, all time')" data-test="project-usage">
                        <x-client.usage-kinds :figures="$usage['total']" />
                        <flux:link class="text-sm" :href="route('usage', ['by' => 'source', 'period' => 'all', 'project' => $project['project_id']])" wire:navigate>{{ __('By source') }}</flux:link>
                    </x-client.section>
                @endif
            </aside>
        </div>
    @endif
</section>
