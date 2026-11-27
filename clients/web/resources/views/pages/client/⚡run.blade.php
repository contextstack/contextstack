<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * One run: its evidence (CLIENT-API.md §5), its usage, and the decision about its result (§6.1). A decision
 * records a judgement about this run's result and authorises nothing further; the receipt says so.
 */
new #[Layout('layouts::client')] #[Title('Run')] class extends DirectorPage
{
    /** Text evidence shown inline at most this many bytes; the whole file is a link away. */
    private const int InlineBytes = 65536;

    #[Locked]
    public string $runId = '';

    public string $note = '';

    #[Locked]
    public string $decisionKey = '';

    /** @var array<string, mixed> */
    #[Locked]
    public array $receipt = [];

    #[Locked]
    public string $viewing = '';

    #[Locked]
    public string $viewed = '';

    public function mount(string $run): void
    {
        $this->runId = $run;
        $this->decisionKey = DirectorClient::newKey();
        $this->refetch();
        abort_if($this->refusedStatus === 404, 404);
    }

    public function refetch(): void
    {
        $run = $this->attempt(fn () => $this->director()->get('runs/'.rawurlencode($this->runId)));
        if (! is_array($run)) {
            return;
        }
        $this->hold('run', $this->runId, $run);
        $this->afterChanges();
    }

    protected function follows(array $change): bool
    {
        return in_array($change['kind'] ?? null, ['run', 'decision'], true)
            && ((($change['kind'] === 'run') ? ($change['id'] ?? null) : ($change['record']['run_id'] ?? null)) === $this->runId);
    }

    protected function afterChanges(): void
    {
        $decisionId = $this->currentRun()['decision']['decision_id'] ?? null;
        if (is_string($decisionId) && ($this->receipt['decision_id'] ?? null) !== $decisionId) {
            $receipt = $this->attempt(fn () => $this->director()->find('runs/'.rawurlencode($this->runId).'/decision'));
            if (is_array($receipt)) {
                $this->receipt = $receipt;
            }
        }

        $held = $this->held('decision', (string) $decisionId);
        if ($held !== null) {
            $this->receipt = $held;
        }
    }

    public function approve(): void
    {
        $this->decide('approve');
    }

    public function reject(): void
    {
        $this->decide('reject');
    }

    public function showFile(string $path): void
    {
        abort_unless(in_array($path, array_column($this->files(), 'path'), true), 404);

        $answer = $this->attempt(fn () => $this->director()->file('runs', $this->runId, $path));
        if ($answer === null) {
            return;
        }
        if ($answer->status() !== 200) {
            $this->problem = __('The Director answered :status for that file.', ['status' => $answer->status()]);

            return;
        }

        $this->viewing = $path;
        $type = (string) $answer->header('Content-Type');
        $body = $answer->body();
        $this->viewed = str_starts_with($type, 'application/octet-stream') || ! mb_check_encoding($body, 'UTF-8')
            ? __('Not text: download it instead.')
            : (strlen($body) > self::InlineBytes ? mb_strcut($body, 0, self::InlineBytes, 'UTF-8')."\n…" : $body);
    }

    private function decide(string $decision): void
    {
        $this->validate(['note' => ['nullable', 'string', 'max:4000']]);

        try {
            $answer = $this->director()->write('runs/'.rawurlencode($this->runId).'/decision',
                array_filter(['decision' => $decision, 'note' => $this->note]), $this->decisionKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Deciding again is safe: it replays the same request.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('already_decided') && is_array($refused->body['decision'] ?? null)) {
                $this->receipt = $refused->body['decision'];
                $this->refetch();
                $this->problem = __('This run was already decided; the receipt below is the decision on record.');

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
    public function currentRun(): ?array
    {
        return $this->held('run', $this->runId);
    }

    /**
     * @return list<array{path: string, bytes: int|null}>
     */
    public function files(): array
    {
        $files = [];
        foreach ((array) ($this->currentRun()['files'] ?? []) as $file) {
            if (is_array($file) && is_string($file['path'] ?? null)) {
                $files[] = ['path' => $file['path'], 'bytes' => is_int($file['bytes'] ?? null) ? $file['bytes'] : null];
            } elseif (is_string($file)) {
                $files[] = ['path' => $file, 'bytes' => null];
            }
        }

        return $files;
    }
};
?>

<section class="mx-auto flex w-full max-w-6xl flex-col gap-6" data-test="run-page"
         x-data="directorStream(@js(route('stream')))">
    @php($run = $this->currentRun())

    <x-client.page-header :back="$run && is_string($run['intent_id'] ?? null) ? route('intents.show', $run['intent_id']) : route('intents.index')"
                          :back-label="$run && is_string($run['intent_id'] ?? null) ? __('Task') : __('Tasks')">
        <span class="font-mono text-base" data-test="run-id">{{ $run['run_id'] ?? __('Run') }}</span>
        <x-slot:actions>
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    @if ($problem !== '')
        <flux:callout variant="warning" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($run)
        <div class="grid gap-x-10 gap-y-6 lg:grid-cols-[minmax(0,1fr)_19rem]">
            <div class="min-w-0 space-y-6">
                @if ($receipt !== [])
                    <x-client.section :heading="__('Decision about this result')" data-test="decision">
                        <div class="space-y-2" data-test="receipt" data-decision="{{ $receipt['decision'] }}">
                            <div class="flex flex-wrap items-center gap-2 text-sm">
                                <flux:badge size="sm" :color="$receipt['decision'] === 'approve' ? 'green' : 'red'">{{ $receipt['decision'] === 'approve' ? __('Approved') : __('Rejected') }}</flux:badge>
                                <span class="text-zinc-500">{{ __('By') }}</span> {{ $receipt['decided_by_name'] ?? $receipt['decided_by'] ?? '—' }}
                                <span class="text-zinc-500">· <x-client.time :at="$receipt['decided_at']" :fallback="'—'" /></span>
                                <span class="text-zinc-500">· {{ __('Receipt') }}</span> <span class="font-mono text-xs">{{ $receipt['decision_id'] }}</span>
                                <span class="text-zinc-500">· {{ __('Scope') }}</span> {{ $receipt['scope'] ?? 'run_result' }}
                            </div>
                            @if (filled($receipt['note'] ?? null))
                                <p class="text-sm"><span class="text-zinc-500">{{ __('Note') }}:</span> {{ $receipt['note'] }}</p>
                            @endif
                            <p class="text-xs text-zinc-500" data-test="authorises">
                                {{ ($receipt['authorises'] ?? []) === [] ? __('This records a judgement about this run\'s result. It authorises nothing further: any later action needs its own authorisation.') : json_encode($receipt['authorises']) }}
                            </p>
                        </div>
                    </x-client.section>
                @elseif (($run['land']['state'] ?? null) === 'landed')
                    <x-client.section :heading="__('Decision about this result')" data-test="decision">
                        <p class="text-sm" data-test="landed-undecided">{{ __('This result landed on :main as :commit without a decision.', ['main' => basename((string) ($run['land']['main'] ?? '')) ?: 'main', 'commit' => substr((string) ($run['land']['commit'] ?? ''), 0, 9)]) }}</p>
                    </x-client.section>
                @else
                    <x-client.needs-action :heading="__('Decision about this result')" data-test="decision">
                        <flux:textarea wire:model="note" :aria-label="__('Note on the decision (optional)')" :placeholder="__('Note (optional)')" rows="1" resize="vertical" />
                        @allowed($run, 'decide', 'decide_run')
                        <div class="flex gap-2">
                            <flux:button size="sm" variant="primary" icon="check" wire:click="approve">{{ __('Approve') }}</flux:button>
                            <flux:button size="sm" variant="danger" icon="x-mark" wire:click="reject">{{ __('Reject') }}</flux:button>
                        </div>
                        @endallowed
                    </x-client.needs-action>
                @endif

                <x-client.section :heading="__('Evidence')" data-test="evidence">
                    @php($files = $this->files())
                    @if ($files === [])
                        <p class="text-sm text-zinc-500">{{ __('No evidence files.') }}</p>
                    @else
                        <ul class="divide-y divide-black/5 rounded-xl border border-black/[.08] dark:divide-white/5 dark:border-white/[.08]">
                            @foreach ($files as $file)
                                <li class="flex items-center gap-2 px-3 py-1" wire:key="file-{{ $file['path'] }}">
                                    <button type="button" wire:click="showFile(@js($file['path']))"
                                            class="min-w-0 flex-1 truncate py-1 text-left font-mono text-[13px] hover:underline {{ $viewing === $file['path'] ? 'font-semibold' : '' }}">{{ $file['path'] }}</button>
                                    <flux:button size="xs" variant="ghost" wire:click="showFile(@js($file['path']))">{{ __('Show') }}</flux:button>
                                    <flux:button size="xs" variant="ghost" :href="route('runs.files', ['run' => $run['run_id'], 'path' => $file['path']])" target="_blank">{{ __('Raw') }}</flux:button>
                                </li>
                            @endforeach
                        </ul>
                    @endif
                    @if ($viewing !== '')
                        <p class="pt-2 font-mono text-xs text-zinc-500">{{ $viewing }}</p>
                        <pre class="max-h-[36rem] overflow-auto whitespace-pre-wrap rounded-xl bg-black/[.04] p-3 font-mono text-[13px] leading-5 dark:bg-white/[.05]" data-test="evidence-view">{{ $viewed }}</pre>
                    @endif
                </x-client.section>
            </div>

            <aside class="space-y-6 lg:border-s lg:border-black/[.06] lg:ps-6 lg:dark:border-white/[.06]">
                <x-client.facts>
                    <x-client.fact :label="__('Verdict')"><strong data-test="verdict">{{ $run['verdict'] ?? '—' }}</strong></x-client.fact>
                    <x-client.fact :label="__('Machine')" mono>{{ $run['node'] }}</x-client.fact>
                    <x-client.fact :label="__('Received')"><x-client.time :at="$run['received_at']" :fallback="'—'" /></x-client.fact>
                </x-client.facts>

                <x-client.section :heading="__('Usage')" data-test="run-usage">
                    <x-client.usage :usage="$run['usage'] ?? null" />
                    @if (is_array($run['usage'] ?? null))
                        <x-client.facts class="pt-1">
                            @foreach (['input_tokens' => __('Input tokens'), 'output_tokens' => __('Output tokens'), 'cache_read_tokens' => __('Cache read'), 'reasoning_tokens' => __('Reasoning')] as $field => $label)
                                @if (isset($run['usage'][$field]))
                                    <x-client.fact :label="$label" wire:key="usage-{{ $field }}">{{ number_format((int) $run['usage'][$field]) }}</x-client.fact>
                                @endif
                            @endforeach
                            <x-client.fact :label="__('Source')">{{ $run['usage']['source']['class'] ?? '—' }}</x-client.fact>
                            @if (($run['usage']['source']['class'] ?? null) === 'api')
                                <x-client.fact :label="__('Charged')" data-test="charged">{{ \App\Client\Money::dollars($run['usage']['charged_usd'] ?? 0) }}</x-client.fact>
                            @endif
                        </x-client.facts>
                    @endif
                </x-client.section>
            </aside>
        </div>
    @endif
</section>
