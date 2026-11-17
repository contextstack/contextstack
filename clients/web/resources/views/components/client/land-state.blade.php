@props(['land'])

@php
    $state = is_array($land) ? ($land['state'] ?? null) : null;
    $commit = is_string($land['commit'] ?? null) ? substr($land['commit'], 0, 9) : null;
    $color = match ($state) {
        'landed' => 'green',
        'failed' => 'red',
        'refused' => 'amber',
        default => 'zinc',
    };
    $label = match ($state) {
        'landed' => __('landed'),
        'failed' => __('land failed'),
        'refused' => __('land refused'),
        default => null,
    };
@endphp

@if ($label !== null)
    <flux:badge size="sm" :color="$color" data-test="land-state" data-state="{{ $state }}" :title="$land['reason'] ?? null">
        {{ $label }}@if ($state === 'landed' && $commit) <span class="ms-1 font-mono">{{ $commit }}</span>@endif
    </flux:badge>
@endif
