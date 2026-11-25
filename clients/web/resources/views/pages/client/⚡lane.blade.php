<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\ListRecords;
use App\Client\LiveUpdates;
use App\Client\RunEvidence;
use App\Livewire\ApprovalsPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * One coding lane (job): live output, result summaries, cancel, decide, and land instructions, and the requests its
 * agent made beyond the sandbox (action approvals), with Approve and Deny for one that waits on the owner.
 */
new #[Layout('layouts::client')] #[Title('Task lane')] class extends ApprovalsPage
{
    #[Locked]
    public string $jobId = '';

    /** @var array<string, array{seq: int, text: string}> */
    #[Locked]
    public array $output = [];

    public string $cancelReason = '';

    #[Locked]
    public string $cancelKey = '';

    #[Locked]
    public string $decisionKey = '';

    public string $decisionNote = '';

    /** @var array<string, mixed> */
    #[Locked]
    public array $receipt = [];

    /** @var array{files: list<string>, added: int, removed: int}|null */
    #[Locked]
    public ?array $diff = null;

    #[Locked]
    public string $lastMessage = '';

    /** Whether the transcript was read (its final message may be empty): it is not downloaded again. */
    #[Locked]
    public bool $transcriptRead = false;

    /**
     * The run the decision receipt, diff stat and last message belong to. When the lane's primary run changes (a
     * later trial reports), they are read again for the new run, so the owner never decides on another run's evidence.
     */
    #[Locked]
    public string $evidenceRunId = '';

    /**
     * Every job on this lane, oldest first, when there is more than one: a retry is another job on the same intent.
     *
     * @var list<array{job_id: string, state: string}>
     */
    #[Locked]
    public array $attempts = [];

    public function mount(string $job): void
    {
        $this->jobId = $job;
        $this->cancelKey = DirectorClient::newKey();
        $this->decisionKey = DirectorClient::newKey();
        $this->refetch();
        abort_if($this->refusedStatus === 404, 404);
    }

    public function refetch(): void
    {
        $job = $this->attempt(fn () => $this->director()->get('jobs/'.rawurlencode($this->jobId)));
        if (! is_array($job)) {
            return;
        }
        $this->hold('job', $this->jobId, (array) ($job['job'] ?? $job));
        $this->loadApprovals(['job' => $this->jobId]);

        $intentId = (string) (($job['job'] ?? $job)['intent_id'] ?? '');
        if ($intentId !== '') {
            $bundle = $this->attempt(fn () => $this->director()->get('intents/'.rawurlencode($intentId)));
            if (is_array($bundle)) {
                $this->hold('intent', $intentId, (array) $bundle['intent']);
                $attempts = ListRecords::lanes(array_values(array_filter((array) ($bundle['jobs'] ?? []), 'is_array')))[0]['attempts'] ?? [];
                $this->attempts = count($attempts) > 1
                    ? array_map(fn (array $attempt): array => ['job_id' => (string) ($attempt['job_id'] ?? ''), 'state' => (string) ($attempt['state'] ?? '')], $attempts)
                    : [];
                foreach ((array) ($bundle['runs'] ?? []) as $run) {
                    $this->hold('run', (string) $run['run_id'], $run);
                }
            }
        }
        $this->afterChanges();
    }

    protected function follows(array $change): bool
    {
        $record = (array) ($change['record'] ?? []);

        return match ($change['kind'] ?? null) {
            'job' => ($change['id'] ?? null) === $this->jobId,
            'intent' => ($change['id'] ?? null) === $this->intentId(),
            'run', 'decision' => ($record['job_id'] ?? null) === $this->jobId,
            'cancel' => ($record['job_id'] ?? null) === $this->jobId,
            default => false,
        };
    }

    protected function followsApproval(array $change): bool
    {
        return ($change['record']['job_id'] ?? null) === $this->jobId;
    }

    protected function afterChanges(): void
    {
        $job = $this->job();
        if ($job === null) {
            return;
        }

        $cancelId = $job['cancel']['cancel_id'] ?? null;
        $heldCancel = is_string($cancelId) ? $this->held('cancel', $cancelId) : null;
        if (is_string($cancelId) && ($heldCancel === null || ($heldCancel['state'] ?? null) !== ($job['cancel']['state'] ?? null))) {
            $cancel = $this->attempt(fn () => $this->director()->get('cancels/'.rawurlencode($cancelId)));
            if (is_array($cancel) && is_int($cancel['revision'] ?? null)) {
                $this->records = LiveUpdates::applyChange($this->records, ['kind' => 'cancel', 'id' => $cancelId,
                    'revision' => $cancel['revision'], 'record' => $cancel]) ?? $this->records;
            }
        }

        // Live text only for runs still in progress: a reported run shows its result instead.
        $live = array_values(array_filter((array) ($job['live_run_ids'] ?? []), fn (mixed $runId): bool => is_string($runId) && $this->held('run', $runId) === null));
        $this->output = array_intersect_key($this->output, array_flip($live));
        foreach ($live as $runId) {
            if (! isset($this->output[$runId])) {
                $this->catchUp($runId);
            }
        }

        $run = $this->primaryRun();
        if ($run === null) {
            return;
        }

        if ($this->evidenceRunId !== (string) $run['run_id']) {
            $this->evidenceRunId = (string) $run['run_id'];
            $this->receipt = [];
            $this->diff = null;
            $this->lastMessage = '';
            $this->transcriptRead = false;
        }

        $decisionId = $run['decision']['decision_id'] ?? null;
        if (is_string($decisionId) && ($this->receipt['decision_id'] ?? null) !== $decisionId) {
            $receipt = $this->attempt(fn () => $this->director()->find('runs/'.rawurlencode((string) $run['run_id']).'/decision'));
            if (is_array($receipt)) {
                $this->receipt = $receipt;
            }
        }

        $paths = array_column((array) ($run['files'] ?? []), 'path');
        if ($this->diff === null && in_array('evidence/candidate.diff', $paths, true)) {
            $bytes = $this->attempt(fn () => $this->evidence((string) $run['run_id'], 'evidence/candidate.diff'));
            if (is_string($bytes)) {
                $this->diff = RunEvidence::diffStat($bytes);
            }
        }
        if (! $this->transcriptRead && in_array('transcript.md', $paths, true)) {
            $bytes = $this->attempt(fn () => $this->evidence((string) $run['run_id'], 'transcript.md'));
            if (is_string($bytes)) {
                $this->lastMessage = RunEvidence::finalMessage($bytes);
                $this->transcriptRead = true;
            }
        }
    }

    protected function applyRunEvents(array $events): void
    {
        foreach ($events as $event) {
            if (($event['job_id'] ?? null) !== $this->jobId || ! is_string($event['run_id'] ?? null)) {
                continue;
            }
            $runId = $event['run_id'];
            if (! isset($this->output[$runId])) {
                $this->catchUp($runId);
            }
            if (isset($this->output[$runId])) {
                $this->output[$runId] = LiveUpdates::tail(LiveUpdates::appendOutput($this->output[$runId], [$event]));
            }
        }
    }

    public function requestCancel(): void
    {
        $this->validate(['cancelReason' => ['nullable', 'string', 'max:2000']]);

        try {
            $answer = $this->director()->write('jobs/'.rawurlencode($this->jobId).'/cancel', array_filter(['reason' => $this->cancelReason]), $this->cancelKey);
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

    public function approve(): void
    {
        $this->decide('approve');
    }

    public function reject(): void
    {
        $this->decide('reject');
    }

    private function decide(string $decision): void
    {
        $run = $this->primaryRun();
        abort_if($run === null, 404);

        $this->validate(['decisionNote' => ['nullable', 'string', 'max:4000']]);
        $runId = (string) $run['run_id'];

        try {
            $answer = $this->director()->write('runs/'.rawurlencode($runId).'/decision',
                array_filter(['decision' => $decision, 'note' => $this->decisionNote]), $this->decisionKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Deciding again is safe.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('already_decided') && is_array($refused->body['decision'] ?? null)) {
                $this->receipt = $refused->body['decision'];
                $this->refetch();

                return;
            }
            if ($refused->is('idempotency_conflict')) {
                $this->decisionKey = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return;
        }

        $this->problem = '';
        $this->receipt = $answer->body;
        $this->decisionKey = DirectorClient::newKey();
        $this->refetch();
    }

    /**
     * @return array<string, mixed>|null
     */
    public function job(): ?array
    {
        return $this->held('job', $this->jobId);
    }

    /**
     * @return array<string, mixed>|null
     */
    public function intent(): ?array
    {
        $id = $this->intentId();

        return $id !== '' ? $this->held('intent', $id) : null;
    }

    public function intentId(): string
    {
        return (string) ($this->job()['intent_id'] ?? '');
    }

    /**
     * @return array<string, mixed>
     */
    public function meta(): array
    {
        return (array) ($this->intent()['meta'] ?? []);
    }

    /**
     * Which model the coding job runs on and, when `start_task model: "auto"` chose it, why: the job's
     * `meta.route` (`{model, reason, languages}`), falling back to the intent's `meta.model`.
     *
     * @return array{model: string, reason: string}
     */
    public function route(): array
    {
        $route = $this->job()['meta']['route'] ?? null;
        $route = is_array($route) ? $route : [];
        $model = is_string($route['model'] ?? null) && $route['model'] !== '' ? $route['model'] : ($this->meta()['model'] ?? '');

        return [
            'model' => is_string($model) ? $model : '',
            'reason' => is_string($route['reason'] ?? null) ? $route['reason'] : '',
        ];
    }

    /**
     * What happened when this lane was landed (CLIENT-API.md §6.4), or null before anyone landed it.
     *
     * @return array<string, mixed>|null
     */
    public function landRecord(): ?array
    {
        $land = $this->job()['land'] ?? null;

        return is_array($land) ? $land : null;
    }

    /**
     * The branch the lane landed on. Records written by early `cs land` builds hold the checkout's folder path in
     * `main`; a branch name never starts with a slash, so those show as the folder's name.
     */
    public function landedBranch(): string
    {
        $main = (string) ($this->landRecord()['main'] ?? '');

        return match (true) {
            $main === '' => 'main',
            str_starts_with($main, '/') => basename($main),
            default => $main,
        };
    }

    public function landed(): bool
    {
        return ($this->landRecord()['state'] ?? null) === 'landed';
    }

    public function landCommand(): ?string
    {
        $repo = (string) ($this->meta()['repo'] ?? '');
        $lane = (string) ($this->meta()['lane'] ?? '');
        if (! preg_match('/\A[a-z0-9._-]+\z/', $repo) || ! preg_match('/\A[a-z0-9._-]+\z/', $lane)) {
            return null;
        }

        return 'cs land '.escapeshellarg($repo).' '.escapeshellarg($lane);
    }

  /**
     * @return array<string, mixed>|null
     */
    public function primaryRun(): ?array
    {
        $job = $this->job();
        if ($job === null) {
            return null;
        }
        $runId = $job['run_id'] ?? null;
        if (is_string($runId)) {
            return $this->held('run', $runId);
        }
        $runIds = array_values(array_filter((array) ($job['run_ids'] ?? []), 'is_string'));
        $last = $runIds !== [] ? $runIds[array_key_last($runIds)] : null;

        return is_string($last) ? $this->held('run', $last) : null;
    }

    /**
     * A run's evidence file, or null when the Director does not give it (a refusal is not the file's contents).
     */
    private function evidence(string $runId, string $path): ?string
    {
        $response = $this->director()->file('runs', $runId, $path);

        return $response->successful() ? $response->body() : null;
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

<section class="mx-auto flex w-full max-w-6xl flex-col gap-6" data-test="lane-page"
         x-data="directorStream(@js(route('stream')))">
    @php($job = $this->job())
    @php($intent = $this->intent())
    @php($run = $this->primaryRun())
    @php($meta = $this->meta())

    <x-client.page-header :back="route('lanes.index')" :back-label="__('Task lanes')">
        <span data-test="lane-title">{{ $job && $intent ? (filled($intent['title'] ?? null) ? $intent['title'] : $intent['task']) : __('Task lane') }}</span>
        <x-slot:status>
            @if ($job)
                <x-client.job-state :job="$job" />
                <x-client.land-state :land="$this->landRecord()" />
            @endif
        </x-slot:status>
        <x-slot:actions>
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($job && $intent)
        @php($landRecord = $this->landRecord())
        @php($land = $this->landCommand())
        @php($showLand = $land && ! $this->landed() && (is_array($landRecord) || (in_array($job['state'], ['done'], true) && (($receipt['decision'] ?? null) === 'approve' || ($run['verdict'] ?? null) === 'pass'))))

        <div class="grid gap-x-10 gap-y-6 lg:grid-cols-[minmax(0,1fr)_19rem]">
            <div class="min-w-0 space-y-6">
                {{-- What waits on you comes first: a request beyond the sandbox, the decision, then landing. --}}
                @foreach ($this->approvalsWaiting() as $approval)
                    <x-client.action-approval :approval="$approval" :show-job="false" />
                @endforeach

                @if ($run && ($receipt !== [] || ! $this->landed()))
                    @if ($receipt !== [])
                        <div class="flex items-center gap-2 text-sm" data-test="lane-decision">
                            <span class="text-zinc-500">{{ __('Approve or reject') }}:</span>
                            <flux:badge size="sm" :color="$receipt['decision'] === 'approve' ? 'green' : 'red'" data-test="decision-receipt">{{ $receipt['decision'] }}</flux:badge>
                        </div>
                    @else
                        <x-client.needs-action :heading="__('Approve or reject')" data-test="lane-decision">
                            <flux:textarea wire:model="decisionNote" :aria-label="__('Note on the decision (optional)')" :placeholder="__('Note (optional)')" rows="1" resize="vertical" />
                            <div class="flex gap-2">
                                <flux:button size="sm" variant="primary" icon="check" wire:click="approve">{{ __('Approve') }}</flux:button>
                                <flux:button size="sm" variant="danger" icon="x-mark" wire:click="reject">{{ __('Reject') }}</flux:button>
                            </div>
                        </x-client.needs-action>
                    @endif
                @endif

                @if ($showLand)
                    <x-client.needs-action :heading="__('Land on main')" data-test="lane-land">
                        @if (is_array($landRecord))
                            <flux:callout :variant="($landRecord['state'] ?? null) === 'refused' ? 'warning' : 'danger'" icon="exclamation-triangle" data-test="land-problem" data-state="{{ $landRecord['state'] ?? '' }}">
                                <flux:callout.heading>{{ ($landRecord['state'] ?? null) === 'refused' ? __('Landing was refused') : __('Landing failed') }}</flux:callout.heading>
                                <flux:callout.text>{{ $landRecord['reason'] ?? __('No reason was recorded.') }}</flux:callout.text>
                            </flux:callout>
                        @endif
                        <p class="text-sm text-zinc-600 dark:text-zinc-400">{{ __('Landing runs on the machine with cs land. When it finishes, the result shows here.') }}</p>
                        <div class="flex flex-wrap items-center gap-2" x-data="{ command: @js($land), copied: false, async copy() { if (await csCopy(this.command)) { this.copied = true; setTimeout(() => this.copied = false, 2000); } } }">
                            <code class="rounded-md bg-black/5 px-2.5 py-1.5 font-mono text-sm dark:bg-white/[.07]" data-test="land-command">{{ $land }}</code>
                            <flux:button size="sm" variant="ghost" icon="clipboard" type="button" x-on:click="copy()" x-bind:data-test="copied ? 'land-copied' : 'land-copy'">
                                <span x-text="copied ? @js(__('Copied')) : @js(__('Copy'))"></span>
                            </flux:button>
                        </div>
                    </x-client.needs-action>
                @endif

                @if ($this->landed())
                    <x-client.section data-test="lane-landed">
                        <div class="flex flex-wrap items-center gap-2">
                            <flux:icon.check-circle variant="mini" class="text-emerald-500" />
                            <h2 class="text-sm font-medium">{{ __('Landed on :main', ['main' => $this->landedBranch()]) }}</h2>
                        </div>
                        <p class="text-sm text-zinc-500">
                            {{ __('Commit') }}: <span class="font-mono text-zinc-800 dark:text-zinc-200" data-test="land-commit">{{ $landRecord['commit'] ?? '—' }}</span>
                            · {{ __('By') }}: {{ $landRecord['landed_by'] ?? '—' }}
                            · {{ __('At') }}: <x-client.time :at="$landRecord['landed_at'] ?? null" :fallback="'—'" />
                        </p>
                    </x-client.section>
                @endif

                @php($otherApprovals = array_values(array_filter($this->approvals, fn (array $approval): bool => ($approval['status'] ?? null) !== 'needs_owner')))
                @if ($otherApprovals !== [])
                    <x-client.section :heading="__('Requests beyond the sandbox')" data-test="lane-approvals">
                        <div class="space-y-3">
                            @foreach ($otherApprovals as $approval)
                                <x-client.action-approval :approval="$approval" :show-job="false" />
                            @endforeach
                        </div>
                    </x-client.section>
                @endif

                @foreach ((array) ($job['live_run_ids'] ?? []) as $runId)
                    @php($reported = $this->held('run', $runId))
                    <x-client.section :heading="__('Live output')" wire:key="live-{{ $runId }}" data-test="live-output" data-run="{{ $runId }}">
                        @if ($reported)
                            <p class="text-sm" data-test="live-replaced">{{ __('The run finished; the result is below.') }}</p>
                        @else
                            <pre class="max-h-[28rem] overflow-auto whitespace-pre-wrap rounded-xl bg-black/[.04] p-3 font-mono text-[13px] leading-5 dark:bg-white/[.05]" data-test="live-text">{{ $output[$runId]['text'] ?? '' }}</pre>
                        @endif
                    </x-client.section>
                @endforeach

                @if ($run)
                    <div class="space-y-6" data-test="lane-result">
                        @if ($lastMessage !== '')
                            <x-client.section :heading="__('The agent\'s last message')" data-test="last-message">
                                <x-client.markdown :text="$lastMessage" class="!text-[15px] !leading-7" />
                            </x-client.section>
                        @endif

                        @if ($diff !== null)
                            <x-client.section :heading="trans_choice(':count file touched|:count files touched', count($diff['files'])).' · '.__('+:added / -:removed lines', ['added' => $diff['added'], 'removed' => $diff['removed']])" data-test="diff-stat">
                                @if ($diff['files'] !== [])
                                    <ul class="divide-y divide-black/5 rounded-xl border border-black/[.08] font-mono text-[13px] dark:divide-white/5 dark:border-white/[.08]">
                                        @foreach ($diff['files'] as $path)
                                            <li class="truncate px-3 py-1.5" wire:key="file-{{ $path }}">{{ $path }}</li>
                                        @endforeach
                                    </ul>
                                @endif
                            </x-client.section>
                        @endif
                    </div>
                @endif
            </div>

            <aside class="space-y-6 lg:border-s lg:border-black/[.06] lg:ps-6 lg:dark:border-white/[.06]" data-test="lane-state">
                <x-client.facts>
                    @php($route = $this->route())
                    <x-client.fact :label="__('Model')" mono>{{ $route['model'] !== '' ? $route['model'] : '—' }}</x-client.fact>
                    @if ($route['reason'] !== '')
                        <x-client.fact :label="__('Model routing')" data-test="model-routing">{{ $route['reason'] }}</x-client.fact>
                    @endif
                    <x-client.fact :label="__('Machine')" mono>{{ $job['node'] }}</x-client.fact>
                    @if (filled($meta['repo'] ?? null))
                        <x-client.fact :label="__('Lane')" mono title="{{ $meta['repo'] }}/{{ $meta['lane'] }}">{{ $meta['repo'] }}/{{ $meta['lane'] }}</x-client.fact>
                    @else
                        <x-client.fact :label="__('Where')">{{ __('scratch workspace') }}</x-client.fact>
                    @endif
                    @if ($run)
                        <x-client.fact :label="__('Check verdict')" data-test="verdict"><strong>{{ $run['verdict'] ?? '—' }}</strong></x-client.fact>
                        <x-client.fact :label="__('Run')" mono title="{{ $run['run_id'] }}"><flux:link :href="route('runs.show', $run['run_id'])" wire:navigate>{{ $run['run_id'] }}</flux:link></x-client.fact>
                    @endif
                    <x-client.fact :label="__('Started')" data-test="lane-times"><x-client.time :at="$job['created_at'] ?? null" :fallback="'—'" /></x-client.fact>
                    @if (filled($job['finished_at'] ?? null))
                        <x-client.fact :label="__('Finished')"><x-client.time :at="$job['finished_at']" /></x-client.fact>
                    @endif
                    <x-client.fact :label="__('Task lane')" mono title="{{ $job['job_id'] }}">{{ $job['job_id'] }}</x-client.fact>
                    @if ($attempts !== [])
                        <x-client.fact :label="__('Attempts')" data-test="lane-attempts">
                            @foreach ($attempts as $number => $attempt)
                                <span class="whitespace-nowrap" wire:key="attempt-{{ $attempt['job_id'] }}">
                                    @if ($attempt['job_id'] === $jobId)
                                        <strong data-test="this-attempt">#{{ $number + 1 }} {{ $attempt['state'] }}</strong>
                                    @else
                                        <flux:link :href="route('lanes.show', $attempt['job_id'])" wire:navigate>#{{ $number + 1 }} {{ $attempt['state'] }}</flux:link>
                                    @endif
                                </span>@if (! $loop->last)<span class="text-zinc-400"> · </span>@endif
                            @endforeach
                        </x-client.fact>
                    @endif
                </x-client.facts>

                @php($cancelId = $job['cancel']['cancel_id'] ?? null)
                @if (is_string($cancelId))
                    @php($cancel = $this->held('cancel', $cancelId) ?? $job['cancel'])
                    <p class="text-sm" data-test="cancel" data-state="{{ $cancel['state'] }}">{{ __('Cancel') }}: {{ $cancel['state'] }}</p>
                @elseif (! in_array($job['state'], ['done', 'error'], true))
                    <form wire:submit="requestCancel" class="space-y-2">
                        <flux:input size="sm" wire:model="cancelReason" :aria-label="__('Reason for cancelling (optional)')" :placeholder="__('Reason (optional)')" />
                        <flux:button size="sm" type="submit" variant="danger" icon="stop">{{ __('Cancel') }}</flux:button>
                    </form>
                @endif
            </aside>
        </div>
    @endif
</section>
