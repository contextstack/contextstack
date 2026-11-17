<?php

use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Attributes\Url;

/*
 * Usage (CLIENT-API.md §12.4), added up by source, project, conversation, intent or node, over a day, a month or
 * all time. Each source is shown for what it is: subscription (usage and allowance), pay as you go (dollars
 * charged), local (usage only). API-price estimates are never shown: they are not charges. Refetched when a run
 * changes on the stream.
 */
new #[Layout('layouts::client')] #[Title('Usage')] class extends DirectorPage
{
    private const array Groupings = ['source', 'project', 'conversation', 'intent', 'node'];

    #[Url]
    public string $by = 'source';

    #[Url]
    public string $period = 'day';

    #[Url]
    public string $date = '';

    #[Url]
    public string $project = '';

    #[Url]
    public string $conversation = '';

    #[Url]
    public string $intent = '';

    #[Url]
    public string $node = '';

    /** @var array<string, mixed> */
    #[Locked]
    public array $usage = [];

    /** @var array<string, string> project id => name */
    #[Locked]
    public array $projectNames = [];

    public function mount(): void
    {
        $projects = $this->attempt(fn () => $this->director()->get('projects'));
        foreach ((array) ($projects['projects'] ?? []) as $project) {
            if (is_array($project) && is_string($project['project_id'] ?? null)) {
                $this->projectNames[$project['project_id']] = (string) ($project['name'] ?? $project['project_id']);
            }
        }
        $this->refetch();
    }

    public function updated(string $property): void
    {
        if (in_array($property, ['by', 'period', 'date', 'project', 'conversation', 'intent', 'node'], true)) {
            $this->refetch();
        }
    }

    public function refetch(): void
    {
        $this->by = in_array($this->by, self::Groupings, true) ? $this->by : 'source';
        $this->period = in_array($this->period, ['day', 'month', 'all'], true) ? $this->period : 'day';

        $answer = $this->attempt(fn () => $this->director()->get('usage', [
            'by' => $this->by, 'period' => $this->period, 'date' => $this->period === 'all' ? null : $this->date,
            'project' => $this->project, 'conversation' => $this->conversation, 'intent' => $this->intent, 'node' => $this->node,
        ]));
        if (is_array($answer)) {
            $this->usage = self::withoutEstimates($answer);
        }
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'run';
    }

    protected function afterChanges(): void
    {
        $this->records = [];
        $this->refetch();
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function groups(): array
    {
        return array_values(array_filter((array) ($this->usage['groups'] ?? []), 'is_array'));
    }

    /**
     * @return array<string, string>
     */
    public function filters(): array
    {
        return array_filter(['project' => $this->project, 'conversation' => $this->conversation, 'intent' => $this->intent, 'node' => $this->node]);
    }

    /**
     * What a grouping or filter field is called on screen: a task (not an intent) and a machine (not a node), as
     * GLOSSARY.md has it; the Client API keeps its own names.
     */
    public function label(string $field): string
    {
        return match ($field) {
            'intent' => __('task'),
            'node' => __('machine'),
            default => __($field),
        };
    }

    public function clearFilters(): void
    {
        $this->reset('project', 'conversation', 'intent', 'node');
        $this->refetch();
    }
};
?>

<section class="mx-auto flex w-full max-w-6xl flex-col gap-5" data-test="usage-page"
         x-data="directorStream(@js(route('stream')))">
    <header class="flex items-start justify-between gap-4 max-sm:flex-col max-sm:gap-3">
        <div>
            <flux:heading size="lg" level="1" class="text-lg!">{{ __('Usage') }}</flux:heading>
            <flux:text class="mt-0.5 max-w-3xl">{{ __('Each source is shown for what it is: pay as you go with the dollars actually charged, a subscription with its usage and the allowance its agent reported, local with usage only. API-price estimates are never shown; they are not charges.') }}</flux:text>
        </div>
        <x-client.stream-dot />
    </header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    <div class="flex flex-wrap items-end gap-3">
        <flux:select wire:model.live="by" :label="__('By')" class="w-40">
            <flux:select.option value="source">{{ __('source') }}</flux:select.option>
            <flux:select.option value="project">{{ __('project') }}</flux:select.option>
            <flux:select.option value="conversation">{{ __('conversation') }}</flux:select.option>
            <flux:select.option value="intent">{{ __('task') }}</flux:select.option>
            <flux:select.option value="node">{{ __('machine') }}</flux:select.option>
        </flux:select>
        <flux:select wire:model.live="period" :label="__('Period')" class="w-36">
            <flux:select.option value="day">{{ __('day') }}</flux:select.option>
            <flux:select.option value="month">{{ __('month') }}</flux:select.option>
            <flux:select.option value="all">{{ __('all') }}</flux:select.option>
        </flux:select>
        @if ($period !== 'all')
            <flux:input wire:model.live.debounce.500ms="date" :label="__('Which')" :placeholder="$period === 'day' ? 'YYYY-MM-DD' : 'YYYY-MM'" class="w-40"
                        :description="__('Blank: the current one, local to the Director\'s host')" />
        @endif
        @if ($this->filters() !== [])
            <div class="flex flex-wrap items-center gap-2" data-test="usage-filters">
                @foreach ($this->filters() as $field => $value)
                    <flux:badge size="sm">{{ $this->label($field) }}: <span class="font-mono">{{ $value }}</span></flux:badge>
                @endforeach
                <flux:button size="xs" variant="ghost" wire:click="clearFilters">{{ __('Clear filters') }}</flux:button>
            </div>
        @endif
    </div>

    @if (isset($usage['from']))
        <flux:text class="text-sm text-zinc-500">{{ __(':from to :to (UTC)', ['from' => $usage['from'], 'to' => $usage['to'] ?? '']) }}</flux:text>
    @endif

    @php($groups = $this->groups())
    @if ($groups === [])
        <flux:text data-test="no-usage">{{ __('No runs in this period.') }}</flux:text>
    @else
        <flux:table>
            <flux:table.columns>
                <flux:table.column>{{ ucfirst($this->label($by)) }}</flux:table.column>
                <flux:table.column>{{ __('Runs') }}</flux:table.column>
                <flux:table.column>{{ __('What it used') }}</flux:table.column>
            </flux:table.columns>
            <flux:table.rows>
                @foreach ($groups as $index => $group)
                    <flux:table.row wire:key="group-{{ $by }}-{{ $index }}" data-test="usage-group">
                        <flux:table.cell>
                            @if ($by === 'source')
                                <span class="font-mono">{{ $group['source']['model'] ?? __('(no model)') }}</span>
                                <span class="text-xs text-zinc-500">{{ $group['source']['class'] ?? __('unknown') }}</span>
                            @else
                                @php($key = $group[$by === 'node' ? 'node' : $by.'_id'] ?? null)
                                @if (! is_string($key) || $key === '')
                                    <span class="text-zinc-500">{{ __('(none)') }}</span>
                                @elseif ($by === 'project')
                                    <flux:link :href="route('projects.show', $key)" wire:navigate>{{ $projectNames[$key] ?? $key }}</flux:link>
                                @elseif ($by === 'conversation')
                                    <flux:link :href="route('conversations.show', $key)" wire:navigate class="font-mono">{{ $key }}</flux:link>
                                @elseif ($by === 'intent')
                                    <flux:link :href="route('intents.show', $key)" wire:navigate class="font-mono">{{ $key }}</flux:link>
                                @else
                                    <span class="font-mono">{{ $key }}</span>
                                @endif
                            @endif
                        </flux:table.cell>
                        <flux:table.cell>{{ (int) ($group['runs'] ?? 0) }}</flux:table.cell>
                        <flux:table.cell>
                            @if ($by === 'source')
                                <x-client.usage-figures :kind="$group['kind'] ?? 'unknown'" :figures="$group" />
                            @else
                                <x-client.usage-kinds :figures="$group" />
                            @endif
                        </flux:table.cell>
                    </flux:table.row>
                @endforeach
            </flux:table.rows>
        </flux:table>

        @if (is_array($usage['total'] ?? null))
            <flux:card class="space-y-2" data-test="usage-total">
                <flux:heading size="lg">{{ __('Total') }}</flux:heading>
                <x-client.usage-kinds :figures="$usage['total']" />
            </flux:card>
        @endif
    @endif
</section>
