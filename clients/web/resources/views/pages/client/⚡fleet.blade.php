<?php

use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Fleet: every node the Director knows (CLIENT-API.md §4.3). A node not heard from lately is shown as stale,
 * never as stopped. Staleness moves without a new revision, so the page also refetches now and then.
 */
new #[Layout('layouts::client')] #[Title('Fleet')] class extends DirectorPage
{
    #[Locked]
    public int $staleAfter = 0;

    public function mount(): void
    {
        $this->refetch();
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get('nodes'));
        if ($answer === null) {
            return;
        }
        $this->records = [];
        $this->staleAfter = (int) ($answer['stale_after_seconds'] ?? 0);
        foreach ((array) ($answer['nodes'] ?? []) as $node) {
            $this->hold('node', (string) $node['name'], $node);
        }
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'node';
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function nodes(): array
    {
        $nodes = $this->heldOf('node');
        usort($nodes, fn (array $a, array $b): int => strcmp((string) $a['name'], (string) $b['name']));

        return $nodes;
    }
};
?>

<section class="mx-auto flex w-full max-w-5xl flex-col gap-5" data-test="fleet-page"
         x-data="directorStream(@js(route('stream')))" wire:poll.30s="refetch">
    <header class="flex items-start justify-between">
        <div>
            <flux:heading size="lg" level="1" class="text-lg!">{{ __('Fleet') }}</flux:heading>
            <flux:text class="mt-0.5">{{ __('Machines pull their work. One not heard from within :seconds seconds is shown as stale.', ['seconds' => $staleAfter]) }}</flux:text>
        </div>
        <x-client.stream-dot />
    </header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    <flux:table>
        <flux:table.columns>
            <flux:table.column>{{ __('Machine') }}</flux:table.column>
            <flux:table.column>{{ __('Seen') }}</flux:table.column>
            <flux:table.column class="max-sm:hidden">{{ __('Protocol') }}</flux:table.column>
            <flux:table.column>{{ __('Open jobs') }}</flux:table.column>
            <flux:table.column class="max-sm:hidden">{{ __('Last status') }}</flux:table.column>
        </flux:table.columns>
        <flux:table.rows>
            @foreach ($this->nodes() as $node)
                <flux:table.row wire:key="node-{{ $node['name'] }}" data-test="node-row">
                    <flux:table.cell>
                        <span class="font-mono">{{ $node['name'] }}</span>
                        @foreach (\App\Client\ProviderLimits::ofNode($node) as $model => $resetsAt)
                            <div class="text-xs text-amber-700 dark:text-amber-400" data-test="node-limit" wire:key="limit-{{ $node['name'] }}-{{ $model }}">
                                <span class="font-mono">{{ $model }}</span> {{ \App\Client\ProviderLimits::phrase($resetsAt) }}
                            </div>
                        @endforeach
                    </flux:table.cell>
                    <flux:table.cell>
                        @if ($node['revoked_at'] ?? null)
                            <flux:badge size="sm" color="red">{{ __('revoked') }}</flux:badge>
                        @elseif ($node['stale'] ?? true)
                            <flux:badge size="sm" color="amber">{{ __('stale') }}</flux:badge>
                        @else
                            <flux:badge size="sm" color="green">{{ ($node['listening'] ?? false) ? __('listening') : __('seen') }}</flux:badge>
                        @endif
                        <span class="text-xs text-zinc-500"><x-client.time :at="$node['last_seen_at']" :fallback="__('never')" /></span>
                    </flux:table.cell>
                    <flux:table.cell class="max-sm:hidden">{{ $node['protocol'] ?? '—' }}</flux:table.cell>
                    <flux:table.cell>{{ $node['open_jobs'] ?? 0 }}</flux:table.cell>
                    <flux:table.cell class="max-sm:hidden text-sm">
                        @if (is_array($node['status'] ?? null))
                            {{ trans_choice(':count active|:count active', count((array) ($node['status']['active'] ?? []))) }}, <x-client.time :at="$node['status']['reported_at']" :fallback="''" />
                        @else
                            —
                        @endif
                    </flux:table.cell>
                </flux:table.row>
            @endforeach
        </flux:table.rows>
    </flux:table>
</section>
