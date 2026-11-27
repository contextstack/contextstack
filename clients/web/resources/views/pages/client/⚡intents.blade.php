<?php

use App\Client\ListRecords;
use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Work: intents newest first with their jobs (CLIENT-API.md §4.2, GET intents), kept current from the stream.
 */
new #[Layout('layouts::client')] #[Title('Tasks')] class extends DirectorPage
{
    /** Tasks shown at first and added by each "Show more": a page holds and renders only these, not every task ever. */
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
        // One more than shown tells whether there is more; a Director without `limit` sends up to 500, cut here.
        $answer = $this->attempt(fn () => $this->director()->get('intents', ['limit' => $this->limit + 1]));
        if ($answer === null) {
            return;
        }

        $this->records = [];
        $intents = array_values(array_filter((array) ($answer['intents'] ?? []), 'is_array'));
        usort($intents, fn (array $a, array $b): int => strcmp((string) ($b['created_at'] ?? ''), (string) ($a['created_at'] ?? '')));
        $this->more = count($intents) > $this->limit;
        $kept = [];
        foreach (array_slice($intents, 0, $this->limit) as $intent) {
            $kept[(string) $intent['intent_id']] = true;
            $this->hold('intent', (string) $intent['intent_id'], $intent);
        }
        foreach ((array) ($answer['jobs'] ?? []) as $job) {
            if (is_array($job) && isset($kept[(string) ($job['intent_id'] ?? '')])) {
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
        if (($change['kind'] ?? null) === 'intent') {
            return true;
        }

        return ($change['kind'] ?? null) === 'job' && is_string($change['record']['intent_id'] ?? null);
    }

    /**
     * @return list<array{intent: array<string, mixed>, jobs: list<array<string, mixed>>}>
     */
    public function rows(): array
    {
        $jobs = $this->heldOf('job');
        $intents = $this->heldOf('intent');
        usort($intents, fn (array $a, array $b): int => strcmp((string) $b['created_at'], (string) $a['created_at']) ?: ((int) $b['revision'] <=> (int) $a['revision']));

        return array_map(fn (array $intent): array => [
            'intent' => $intent,
            'jobs' => array_values(array_filter($jobs, fn (array $job): bool => ($job['intent_id'] ?? null) === $intent['intent_id'])),
        ], $intents);
    }
};
?>

<section class="mx-auto flex w-full max-w-5xl flex-col gap-5" data-test="intents-page"
         x-data="directorStream(@js(route('stream')))">
    <header class="flex items-start justify-between gap-4 max-sm:flex-col max-sm:gap-3">
        <div>
            <flux:heading size="lg" level="1" class="text-lg!">{{ __('Tasks') }}</flux:heading>
            <flux:text class="mt-0.5">{{ __('What you asked for, and the jobs that worked on each.') }}</flux:text>
        </div>
        <div class="flex items-center gap-3">
            <x-client.stream-dot />
            <div class="flex gap-2">
            @capable('start_work')
            <flux:button variant="filled" icon="code-bracket" :href="route('lanes.create')" wire:navigate>{{ __('New task lane') }}</flux:button>
            @endcapable
            @capable('start_work')
            <flux:button variant="primary" icon="plus" :href="route('intents.create')" wire:navigate>{{ __('New task') }}</flux:button>
            @endcapable
        </div>
        </div>
    </header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @php($rows = $this->rows())
    @if ($rows === [])
        <flux:text data-test="no-intents">{{ __('Nothing yet. Submit a task to start.') }}</flux:text>
    @else
        <flux:table>
            <flux:table.columns>
                <flux:table.column>{{ __('Task') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Machine') }}</flux:table.column>
                <flux:table.column>{{ __('Job') }}</flux:table.column>
                <flux:table.column>{{ __('Submitted') }}</flux:table.column>
            </flux:table.columns>
            <flux:table.rows>
                @foreach ($rows as $row)
                    <flux:table.row wire:key="intent-{{ $row['intent']['intent_id'] }}">
                        <flux:table.cell>
                            <flux:link :href="route('intents.show', $row['intent']['intent_id'])" wire:navigate>
                                {{ filled($row['intent']['title'] ?? null) ? $row['intent']['title'] : $row['intent']['task'] }}
                            </flux:link>
                        </flux:table.cell>
                        <flux:table.cell class="font-mono max-sm:hidden">{{ $row['intent']['node'] }}</flux:table.cell>
                        <flux:table.cell>
                            @foreach ($row['jobs'] as $job)
                                <x-client.job-state :job="$job" />
                            @endforeach
                        </flux:table.cell>
                        <flux:table.cell><x-client.time :at="$row['intent']['created_at']" /></flux:table.cell>
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
