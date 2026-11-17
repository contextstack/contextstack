<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\LiveUpdates;
use App\Client\RunAgain;
use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * One intent: its job as it runs, live output while it runs (CLIENT-API.md §5.1, §8.1), its runs with usage, and
 * a cancel request with its states (§6.2). Kept current from the stream by the reconnect order.
 */
new #[Layout('layouts::client')] #[Title('Task')] class extends DirectorPage
{
    #[Locked]
    public string $intentId = '';

    /** @var array<string, array{seq: int, text: string}> live output per run id */
    #[Locked]
    public array $output = [];

    /** @var array<string, mixed> */
    #[Locked]
    public array $usage = [];

    /** @var array<string, string> job id => the reason typed for cancelling that job */
    public array $cancelReasons = [];

    #[Locked]
    public string $cancelKey = '';

    #[Locked]
    public string $againKey = '';

    public string $notice = '';

    public function mount(string $intent): void
    {
        $this->intentId = $intent;
        $this->cancelKey = DirectorClient::newKey();
        $this->againKey = DirectorClient::newKey();
        $this->notice = (string) session('notice', '');
        $this->refetch();
        abort_if($this->refusedStatus === 404, 404);
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get('intents/'.rawurlencode($this->intentId)));
        if ($answer === null) {
            return;
        }

        $this->hold('intent', $this->intentId, (array) $answer['intent']);
        foreach ((array) ($answer['jobs'] ?? []) as $job) {
            $this->hold('job', (string) $job['job_id'], $job);
        }
        foreach ((array) ($answer['runs'] ?? []) as $run) {
            $this->hold('run', (string) $run['run_id'], $run);
        }
        $this->afterChanges();
    }

    protected function follows(array $change): bool
    {
        $record = (array) ($change['record'] ?? []);

        return match ($change['kind'] ?? null) {
            'intent' => ($change['id'] ?? null) === $this->intentId,
            'job' => ($record['intent_id'] ?? null) === $this->intentId,
            'run', 'decision' => ($record['intent_id'] ?? null) === $this->intentId || $this->isOurJob($record['job_id'] ?? null),
            'cancel' => $this->isOurJob($record['job_id'] ?? null),
            default => false,
        };
    }

    protected function afterChanges(): void
    {
        // Live text only for runs still in progress: a reported run shows its result instead.
        $live = [];
        foreach ($this->jobs() as $job) {
            foreach ((array) ($job['live_run_ids'] ?? []) as $runId) {
                if (is_string($runId) && $this->held('run', $runId) === null) {
                    $live[$runId] = true;
                }
            }
        }
        $this->output = array_intersect_key($this->output, $live);

        foreach ($this->jobs() as $job) {
            $cancelId = $job['cancel']['cancel_id'] ?? null;
            $heldCancel = is_string($cancelId) ? $this->held('cancel', $cancelId) : null;
            if (is_string($cancelId) && ($heldCancel === null || ($heldCancel['state'] ?? null) !== ($job['cancel']['state'] ?? null))) {
                $cancel = $this->attempt(fn () => $this->director()->get('cancels/'.rawurlencode($cancelId)));
                if (is_array($cancel) && is_int($cancel['revision'] ?? null)) {
                    $this->records = LiveUpdates::applyChange($this->records, ['kind' => 'cancel', 'id' => $cancelId,
                        'revision' => $cancel['revision'], 'record' => $cancel]) ?? $this->records;
                }
            }
            foreach ((array) ($job['live_run_ids'] ?? []) as $runId) {
                if (isset($live[$runId]) && ! isset($this->output[$runId])) {
                    $this->catchUp($runId);
                }
            }
        }

        $usage = $this->attempt(fn () => $this->director()->get('usage', ['by' => 'intent', 'intent' => $this->intentId, 'period' => 'all']));
        if (is_array($usage)) {
            $this->usage = self::withoutEstimates($usage);
        }
    }

    protected function applyRunEvents(array $events): void
    {
        $byRun = [];
        foreach ($events as $event) {
            if (is_string($event['run_id'] ?? null) && $this->isOurJob($event['job_id'] ?? null)) {
                $byRun[$event['run_id']][] = $event;
            }
        }

        foreach ($byRun as $runId => $runEvents) {
            if (! isset($this->output[$runId])) {
                $this->catchUp($runId);
            }
            if (isset($this->output[$runId])) {
                $this->output[$runId] = LiveUpdates::tail(LiveUpdates::appendOutput($this->output[$runId], $runEvents));
            }
        }
    }

    public function requestCancel(string $jobId): void
    {
        abort_unless($this->isOurJob($jobId), 404);
        $this->validate(['cancelReasons.*' => ['nullable', 'string', 'max:2000']]);
        $reason = (string) ($this->cancelReasons[$jobId] ?? '');

        try {
            $answer = $this->director()->write('jobs/'.rawurlencode($jobId).'/cancel', array_filter(['reason' => $reason]), $this->cancelKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Asking again is safe.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->cancelKey = DirectorClient::newKey();
            }
            $this->refetch();
            $this->problem = $refused->is('job_finished') ? __('The job is already over; there is nothing to cancel.') : $refused->explain();

            return;
        }

        $this->problem = '';
        $this->cancelKey = DirectorClient::newKey();
        $this->records = LiveUpdates::applyChange($this->records, ['kind' => 'cancel', 'id' => (string) $answer->body['cancel_id'],
            'revision' => (int) ($answer->body['revision'] ?? 0), 'record' => $answer->body]) ?? $this->records;
        $this->refetch();
    }

    public function runAgain(string $jobId): void
    {
        $job = $this->held('job', $jobId);
        $intent = $this->intent();
        abort_unless($job !== null && $intent !== null, 404);

        try {
            $answer = $this->director()->write('intents', RunAgain::body($intent, $job), $this->againKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Asking again is safe.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();
            if ($refused->is('idempotency_conflict')) {
                $this->againKey = DirectorClient::newKey();
            }

            return;
        }

        $this->againKey = DirectorClient::newKey();
        if (RunAgain::addedCheck($answer->body)) {
            session()->flash('notice', __('The earlier job\'s bundle had no check, so its runs always ended "fail". This run adds the "no check" check: every run passes.'));
        }
        $this->redirectRoute('intents.show', ['intent' => $answer->body['intent']['intent_id']], navigate: true);
    }

    /**
     * @return array<string, mixed>|null
     */
    public function intent(): ?array
    {
        return $this->held('intent', $this->intentId);
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function jobs(): array
    {
        return $this->heldOf('job');
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function runsOf(string $jobId): array
    {
        return array_values(array_filter($this->heldOf('run'), fn (array $run): bool => ($run['job_id'] ?? null) === $jobId));
    }

    private function isOurJob(mixed $jobId): bool
    {
        return is_string($jobId) && $this->held('job', $jobId) !== null;
    }

    private function catchUp(string $runId): void
    {
        $caught = LiveUpdates::catchUp($this->output[$runId] ?? null, fn (int $after): mixed => $this->attempt(fn () => $this->director()->get('runs/'.rawurlencode($runId).'/events', ['after' => $after])), LiveUpdates::OUTPUT_TAIL);
        if ($caught !== null) {
            $this->output[$runId] = $caught;
        }
    }
};
?>

<section class="mx-auto flex w-full max-w-6xl flex-col gap-6" data-test="intent-page"
         x-data="directorStream(@js(route('stream')))">
    @php($intent = $this->intent())

    <x-client.page-header :back="route('intents.index')" :back-label="__('Tasks')">
        <span data-test="intent-title">{{ $intent ? (filled($intent['title'] ?? null) ? $intent['title'] : $intent['task']) : __('Task') }}</span>
        <x-slot:actions>
            @if ($intent)
                <flux:button size="sm" variant="ghost" icon="chat-bubble-left-right" :href="route('conversations.create', ['intent' => $intent['intent_id']])" wire:navigate>{{ __('Discuss this task') }}</flux:button>
            @endif
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($notice !== '')
        <p class="text-sm text-zinc-600 dark:text-zinc-400" data-test="notice">{{ $notice }}</p>
    @endif

    @if ($intent)
        <div class="grid gap-x-10 gap-y-6 lg:grid-cols-[minmax(0,1fr)_19rem]">
            <div class="min-w-0 space-y-8">
                @foreach ($this->jobs() as $job)
                    <section class="space-y-3" wire:key="job-{{ $job['job_id'] }}" data-test="job">
                        <div class="flex flex-wrap items-center gap-2">
                            <h2 class="text-sm font-medium">{{ __('Job') }}</h2>
                            <x-client.job-state :job="$job" />
                            <span class="font-mono text-xs text-zinc-500">{{ $job['job_id'] }}</span>
                            @if (in_array($job['state'], ['done', 'error'], true))
                                <flux:button size="xs" variant="ghost" icon="arrow-path" class="ms-auto" wire:click="runAgain({{ Js::from($job['job_id']) }})">{{ __('Run again') }}</flux:button>
                            @endif
                        </div>

                        <p class="text-xs text-zinc-500">
                            {{ __('Created') }} <x-client.time :at="$job['created_at']" />
                            · {{ __('Accepted') }} <x-client.time :at="$job['accepted_at']" :fallback="'—'" />
                            · {{ __('Finished') }} <x-client.time :at="$job['finished_at']" :fallback="'—'" />
                            · {{ __('Revision') }} {{ $job['revision'] }}
                        </p>

                        @if (($job['state'] ?? null) === 'waiting' && is_array($job['budget'] ?? null))
                            <p class="flex items-center gap-2 text-sm text-amber-600 dark:text-amber-400"><flux:icon.clock variant="micro" /> {{ $job['budget']['detail'] ?? __('Waiting for budget.') }}</p>
                        @endif

                        @php($cancelId = $job['cancel']['cancel_id'] ?? null)
                        @if (is_string($cancelId))
                            @php($cancel = $this->held('cancel', $cancelId) ?? $job['cancel'])
                            <div class="text-sm" data-test="cancel" data-state="{{ $cancel['state'] }}">
                                <span class="font-medium">{{ __('Cancel') }}:</span>
                                @if (in_array($cancel['state'], ['requested', 'received'], true))
                                    {{ __('stopping') }} ({{ $cancel['state'] === 'requested' ? __('requested; the machine has not picked it up') : __('received by the machine') }})
                                @elseif ($cancel['state'] === 'applied')
                                    {{ ($cancel['outcome'] ?? null) === 'already_finished' ? __('the job was already finished') : __('stopped') }}
                                @else
                                    {{ __('refused by the machine') }}
                                @endif
                                @if (filled($cancel['detail'] ?? null))
                                    <span class="text-zinc-500">· {{ $cancel['detail'] }}</span>
                                @endif
                            </div>
                        @elseif (! in_array($job['state'], ['done', 'error'], true))
                            <form wire:submit="requestCancel({{ Js::from($job['job_id']) }})" class="flex items-center gap-2">
                                <flux:input size="sm" wire:model="cancelReasons.{{ $job['job_id'] }}" :placeholder="__('Reason (optional)')" :aria-label="__('Reason for cancelling (optional)')" class="max-w-xs" />
                                <flux:button size="sm" type="submit" variant="danger" icon="stop">{{ __('Request cancel') }}</flux:button>
                            </form>
                        @endif

                        @foreach ((array) ($job['live_run_ids'] ?? []) as $runId)
                            @php($reported = $this->held('run', $runId))
                            <div wire:key="live-{{ $runId }}" data-test="live-output" data-run="{{ $runId }}">
                                <p class="text-xs text-zinc-500">{{ __('Output of run') }} <span class="font-mono">{{ $runId }}</span></p>
                                @if ($reported)
                                    <p class="text-sm" data-test="live-replaced">{{ __('The run is reported; its evidence is the result.') }}
                                        <flux:link :href="route('runs.show', $runId)" wire:navigate>{{ __('Open the result') }}</flux:link></p>
                                @else
                                    <pre class="mt-1 max-h-96 overflow-auto whitespace-pre-wrap rounded-xl bg-black/[.04] p-3 font-mono text-[13px] leading-5 dark:bg-white/[.05]" data-test="live-text">{{ $output[$runId]['text'] ?? '' }}</pre>
                                @endif
                            </div>
                        @endforeach

                        @php($runs = $this->runsOf($job['job_id']))
                        @if ($runs !== [])
                            <flux:table>
                                <flux:table.columns>
                                    <flux:table.column>{{ __('Run') }}</flux:table.column>
                                    <flux:table.column>{{ __('Verdict') }}</flux:table.column>
                                    <flux:table.column>{{ __('Usage') }}</flux:table.column>
                                    <flux:table.column>{{ __('Decision') }}</flux:table.column>
                                </flux:table.columns>
                                <flux:table.rows>
                                    @foreach ($runs as $run)
                                        <flux:table.row wire:key="run-{{ $run['run_id'] }}" data-test="run-row">
                                            <flux:table.cell><flux:link :href="route('runs.show', $run['run_id'])" wire:navigate class="font-mono text-[13px]">{{ $run['run_id'] }}</flux:link></flux:table.cell>
                                            <flux:table.cell>{{ $run['verdict'] ?? '—' }}</flux:table.cell>
                                            <flux:table.cell><x-client.usage :usage="$run['usage'] ?? null" /></flux:table.cell>
                                            <flux:table.cell>{{ $run['decision']['decision'] ?? __('not decided') }}</flux:table.cell>
                                        </flux:table.row>
                                    @endforeach
                                </flux:table.rows>
                            </flux:table>
                        @endif
                    </section>
                @endforeach
            </div>

            <aside class="space-y-6 lg:border-s lg:border-black/[.06] lg:ps-6 lg:dark:border-white/[.06]">
                <x-client.facts>
                    <x-client.fact :label="__('Spec')" mono title="{{ $intent['spec'] }}">{{ $intent['spec'] }}</x-client.fact>
                    <x-client.fact :label="__('Task')" mono title="{{ $intent['task'] }}">{{ $intent['task'] }}</x-client.fact>
                    <x-client.fact :label="__('Machine')" mono>{{ $intent['node'] }}</x-client.fact>
                    <x-client.fact :label="__('Trials')">{{ trans_choice(':count trial|:count trials', $intent['trials'] ?? 1) }}</x-client.fact>
                    @if (is_string($intent['project_id'] ?? null))
                        <x-client.fact :label="__('Project')" mono><flux:link :href="route('projects.show', $intent['project_id'])" wire:navigate>{{ $intent['project_id'] }}</flux:link></x-client.fact>
                    @endif
                    <x-client.fact :label="__('ID')" mono title="{{ $intent['intent_id'] }}">{{ $intent['intent_id'] }}</x-client.fact>
                </x-client.facts>

                @if ((array) ($intent['conversation_ids'] ?? []) !== [])
                    <x-client.section :heading="__('Conversations')">
                        @foreach ((array) ($intent['conversation_ids'] ?? []) as $conversationId)
                            <flux:link :href="route('conversations.show', $conversationId)" wire:navigate class="block text-sm" wire:key="intent-conversation-{{ $conversationId }}">{{ __('Conversation :id', ['id' => $conversationId]) }}</flux:link>
                        @endforeach
                    </x-client.section>
                @endif

                @if (is_array($usage['total'] ?? null))
                    <x-client.section :heading="__('Usage of this task')" data-test="intent-usage">
                        <p class="text-sm">
                            {{ trans_choice(':count run|:count runs', (int) ($usage['total']['runs'] ?? 0)) }}@if (isset($usage['total']['charged_usd'])), {{ __('charged') }} {{ \App\Client\Money::dollars($usage['total']['charged_usd']) }}@endif
                        </p>
                        @foreach ((array) ($usage['total']['kinds'] ?? []) as $kind => $figures)
                            @if (is_array($figures) && ($figures['runs'] ?? 0) > 0)
                                <p class="text-sm text-zinc-500" wire:key="usage-kind-{{ $kind }}">{{ str_replace('_', ' ', $kind) }}: {{ $figures['shown'] ?? '' }}</p>
                            @endif
                        @endforeach
                    </x-client.section>
                @endif
            </aside>
        </div>
    @endif
</section>
