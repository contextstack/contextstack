<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\ListRecords;
use Livewire\Attributes\Locked;
use Livewire\Attributes\On;
use Livewire\Component;

/*
 * The lanes a lead conversation started, beside the thread: each lane's state, its land state, and a link to it.
 * A lane belongs to the thread when its intent's lead_conversation_id names the conversation (CLIENT-API.md,
 * GET intents?conversation=ID). The thread's page follows the stream and tells this pane when one of them changes,
 * so the pane opens no stream of its own.
 */
new class extends Component
{
    /** The newest lanes shown: a long-running conversation can start hundreds. */
    public const SHOWN = 100;

    #[Locked]
    public string $conversationId = '';

    /**
     * One entry per lane, newest first: its newest attempt (`job`), every attempt oldest first, and its intent.
     *
     * @var list<array{job: array<string, mixed>, attempts: list<array<string, mixed>>, intent: array<string, mixed>}>
     */
    #[Locked]
    public array $lanes = [];

    public function mount(string $conversationId): void
    {
        $this->conversationId = $conversationId;
        $this->refresh();
    }

    #[On('thread-lanes-changed')]
    public function refresh(): void
    {
        try {
            $answer = app(DirectorClient::class)->get('intents', ['conversation' => $this->conversationId, 'limit' => self::SHOWN]);
        } catch (DirectorRefused|DirectorUnavailable|NotConnected) {
            return;
        }

        $intents = [];
        foreach ((array) ($answer['intents'] ?? []) as $intent) {
            if (is_array($intent) && ($intent['lead_conversation_id'] ?? null) === $this->conversationId && is_string($intent['intent_id'] ?? null)) {
                $intents[$intent['intent_id']] = ListRecords::intent($intent);
            }
        }

        $jobs = [];
        foreach ((array) ($answer['jobs'] ?? []) as $job) {
            if (is_array($job) && isset($intents[$job['intent_id'] ?? ''])) {
                $jobs[] = ListRecords::job($job);
            }
        }

        $this->lanes = array_map(fn (array $lane): array => $lane + ['intent' => $intents[(string) $lane['job']['intent_id']]], ListRecords::lanes($jobs));
    }
};
?>

<div class="-mx-2 grid gap-px" data-test="thread-lanes">
    @forelse ($lanes as $lane)
        @php($meta = (array) ($lane['intent']['meta'] ?? []))
        <a href="{{ route('lanes.show', $lane['job']['job_id']) }}" wire:navigate wire:key="thread-lane-{{ $lane['job']['job_id'] }}"
           class="block rounded-lg px-2 py-2 hover:bg-black/5 dark:hover:bg-white/[.06]"
           data-test="thread-lane" data-job="{{ $lane['job']['job_id'] }}">
            <div class="flex items-center gap-2">
                <span class="min-w-0 flex-1 truncate text-sm">{{ filled($lane['intent']['title'] ?? null) ? $lane['intent']['title'] : ($lane['intent']['task'] ?? $lane['job']['job_id']) }}</span>
                <x-client.job-state :job="$lane['job']" />
            </div>
            @if (count($lane['attempts']) > 1)
                <div class="mt-1 text-xs text-zinc-500" data-test="thread-lane-attempts">
                    {{ __('Attempt :n · earlier: :states', [
                        'n' => count($lane['attempts']),
                        'states' => collect(array_slice($lane['attempts'], 0, -1))->map(fn (array $attempt): string => (string) ($attempt['state'] ?? '?'))->implode(', '),
                    ]) }}
                </div>
            @endif
            @if (filled($meta['lane'] ?? null) || is_array($lane['job']['land'] ?? null))
                <div class="mt-1 flex items-center gap-2">
                    <span class="min-w-0 flex-1 truncate font-mono text-xs text-zinc-500">{{ filled($meta['lane'] ?? null) ? ($meta['repo'] ?? '').'/'.$meta['lane'] : '' }}</span>
                    <x-client.land-state :land="$lane['job']['land'] ?? null" />
                </div>
            @endif
        </a>
    @empty
        <p class="px-2 py-2 text-sm leading-6 text-zinc-500" data-test="no-thread-lanes">{{ __('No task lanes from this conversation yet. Ask for work here and the lanes it starts show up.') }}</p>
    @endforelse
</div>
