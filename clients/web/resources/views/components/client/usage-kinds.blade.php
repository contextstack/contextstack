@props(['figures'])

{{-- A usage group's (or total's) runs, broken down by kind of source, each shown for what it is. --}}
<div class="space-y-1" data-test="usage-kinds">
    @foreach ((array) ($figures['kinds'] ?? []) as $kind => $kindFigures)
        @if (is_array($kindFigures) && ($kindFigures['runs'] ?? 0) > 0)
            <x-client.usage-figures :kind="$kind" :figures="$kindFigures" wire:key="kind-{{ $kind }}" />
        @endif
    @endforeach
    @if ((int) ($figures['runs'] ?? 0) === 0)
        <flux:text class="text-sm">{{ __('No runs.') }}</flux:text>
    @endif
</div>
