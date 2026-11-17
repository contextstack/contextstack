@props(['conversation'])

{{-- A conversation's state (CLIENT-API.md §11.2): idle, running, summarising, or held (stopped; turns queue). --}}
@php
    $state = (string) ($conversation['state'] ?? 'idle');
    [$label, $color] = match ($state) {
        'running' => [__('running'), 'blue'],
        'summarising' => [__('summarising'), 'violet'],
        'held' => [__('stopped: messages queue'), 'amber'],
        default => [__('idle'), 'zinc'],
    };
@endphp

<flux:badge size="sm" :color="$color" data-test="conversation-state" data-state="{{ $state }}">{{ $label }}</flux:badge>
