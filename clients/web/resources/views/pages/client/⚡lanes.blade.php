<?php

use App\Client\ListRecords;
use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Lanes: recent coding jobs (GET intents), kept current from the stream.
 */
new #[Layout('layouts::client')] #[Title('Task lanes')] class extends DirectorPage
{
    /** Lanes shown at first and added by each "Show more": a page holds and renders only these, not every lane ever. */
    public const PAGE = 50;

    #[Locked]
    public int $limit = self::PAGE;

    #[Locked]
    public bool $more = false;

    public function showMore(): void
    {
        $this->limit += self::PAGE;
        $this->refetch();
    }

    public function mount(): void
    {
        $this->refetch();
    }

    public function refetch(): void
    {
        // Only lanes, one more than shown (to tell whether there is more). A Director without `lanes`/`limit` sends
        // every intent, up to 500; the filter and the cut below still hold.
        $answer = $this->attempt(fn () => $this->director()->get('intents', ['lanes' => '1', 'limit' => $this->limit + 1]));
        if ($answer === null) {
            return;
        }

        $this->records = [];
        $laneIntents = [];
        foreach ((array) ($answer['intents'] ?? []) as $intent) {
            if (is_array($intent) && $this->isLaneIntent($intent)) {
                $laneIntents[(string) $intent['intent_id']] = $intent;
            }
        }
        $jobs = array_values(array_filter((array) ($answer['jobs'] ?? []), fn (mixed $job): bool => is_array($job) && isset($laneIntents[(string) ($job['intent_id'] ?? '')])));
        // A retry is another job on the same lane: count and cut by lane, not by job.
        $lanes = ListRecords::lanes($jobs);
        $this->more = count($lanes) > $this->limit;
        foreach (array_slice($lanes, 0, $this->limit) as $lane) {
            $this->hold('intent', (string) $lane['job']['intent_id'], $laneIntents[(string) $lane['job']['intent_id']]);
            foreach ($lane['attempts'] as $job) {
                $this->hold('job', (string) $job['job_id'], $job);
            }
        }
    }

    protected function shown(string $kind, array $record): array
    {
        return ListRecords::of($kind, $record);
    }

    protected function follows(array $change): bool
    {
        return match ($change['kind'] ?? null) {
            'job' => $this->isLaneJob((array) ($change['record'] ?? [])),
            'intent' => $this->isLaneIntent((array) ($change['record'] ?? [])),
            default => false,
        };
    }

    /**
     * One row per lane: its newest attempt, every attempt oldest first, and its intent.
     *
     * @return list<array{job: array<string, mixed>, attempts: list<array<string, mixed>>, intent: array<string, mixed>|null}>
     */
    public function rows(): array
    {
        return array_map(fn (array $lane): array => $lane + [
            'intent' => is_string($lane['job']['intent_id'] ?? null) ? $this->held('intent', $lane['job']['intent_id']) : null,
        ], ListRecords::lanes(array_values($this->heldOf('job'))));
    }

    /**
     * @param  array<string, mixed>  $job
     */
    private function isLaneJob(array $job): bool
    {
        if (! is_string($job['intent_id'] ?? null)) {
            return false;
        }
        $intent = $this->held('intent', $job['intent_id']);

        return $intent !== null && $this->isLaneIntent($intent);
    }

    /**
     * Whether an intent is a task lane: the Director decides (its derived `lane` flag, the same rule as `lanes=1`).
     *
     * @param  array<string, mixed>  $intent
     */
    private function isLaneIntent(array $intent): bool
    {
        return ($intent['lane'] ?? false) === true;
    }
};
?>

<section class="mx-auto flex w-full max-w-5xl flex-col gap-5" data-test="lanes-page"
         x-data="directorStream(@js(route('stream')))">
    <header class="flex items-start justify-between gap-4 max-sm:flex-col max-sm:gap-3">
        <div>
            <flux:heading size="lg" level="1" class="text-lg!">{{ __('Task lanes') }}</flux:heading>
            <flux:text class="mt-0.5">{{ __('Coding-agent jobs on repo worktrees or scratch workspaces.') }}</flux:text>
        </div>
        <div class="flex items-center gap-3">
            <x-client.stream-dot />
            <flux:button variant="primary" icon="plus" :href="route('lanes.create')" wire:navigate>{{ __('New task lane') }}</flux:button>
        </div>
    </header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @php($rows = $this->rows())
    @if ($rows === [])
        <flux:text data-test="no-lanes">{{ __('Nothing yet. Start a task lane to code in a worktree.') }}</flux:text>
    @else
        <flux:table>
            <flux:table.columns>
                <flux:table.column>{{ __('Title') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Repo / lane') }}</flux:table.column>
                <flux:table.column>{{ __('State') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Started') }}</flux:table.column>
            </flux:table.columns>
            <flux:table.rows>
                @foreach ($rows as $row)
                    @php($intent = $row['intent'])
                    @php($meta = is_array($intent) ? (array) ($intent['meta'] ?? []) : [])
                    <flux:table.row wire:key="lane-{{ $row['job']['job_id'] }}">
                        <flux:table.cell>
                            <flux:link :href="route('lanes.show', $row['job']['job_id'])" wire:navigate>
                                {{ filled($intent['title'] ?? null) ? $intent['title'] : ($intent['task'] ?? $row['job']['job_id']) }}
                            </flux:link>
                        </flux:table.cell>
                        <flux:table.cell class="font-mono text-sm max-sm:hidden">
                            @if (filled($meta['repo'] ?? null))
                                {{ $meta['repo'] }}/{{ $meta['lane'] ?? '—' }}
                            @else
                                {{ __('scratch') }}
                            @endif
                        </flux:table.cell>
                        <flux:table.cell>
                            <div class="flex items-center gap-1">
                                <x-client.job-state :job="$row['job']" />
                                <x-client.land-state :land="$row['job']['land'] ?? null" />
                                @if (count($row['attempts']) > 1)
                                    <span class="text-xs text-zinc-500" data-test="lane-attempts">{{ __('attempt :n', ['n' => count($row['attempts'])]) }}</span>
                                @endif
                            </div>
                        </flux:table.cell>
                        <flux:table.cell class="max-sm:hidden"><x-client.time :at="$row['job']['created_at']" /></flux:table.cell>
                    </flux:table.row>
                @endforeach
            </flux:table.rows>
        </flux:table>
        @if ($more)
            <div class="flex justify-center">
                <flux:button size="sm" variant="ghost" wire:click="showMore" data-test="show-more">{{ __('Show more') }}</flux:button>
            </div>
        @endif
    @endif
</section>
