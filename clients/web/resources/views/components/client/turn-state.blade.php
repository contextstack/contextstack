@props(['turn'])

{{-- A turn's state (CLIENT-API.md §11.2). A turn whose job has a cancel still out shows "stopping". --}}
@php
    $state = (string) ($turn['state'] ?? 'queued');
    $cancel = $turn['cancel'] ?? null;
    $stopping = is_array($cancel) && in_array($cancel['state'] ?? null, ['requested', 'received'], true) && ! in_array($state, ['done', 'error', 'stopped'], true);
    [$label, $color] = match (true) {
        $stopping => [__('stopping'), 'amber'],
        $state === 'queued' => [__('queued'), 'zinc'],
        $state === 'summarising' => [__('waiting for a summary'), 'violet'],
        $state === 'running' => [__('running'), 'blue'],
        $state === 'done' => [__('done'), 'green'],
        $state === 'stopped' => [__('stopped'), 'amber'],
        default => [__('error'), 'red'],
    };
@endphp

<flux:badge size="sm" :color="$color" data-test="turn-state" data-state="{{ $state }}">{{ $label }}</flux:badge>
