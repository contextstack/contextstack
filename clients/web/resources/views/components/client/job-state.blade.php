@props(['job'])

@php
    $cancel = $job['cancel'] ?? null;
    $stopping = is_array($cancel) && in_array($cancel['state'] ?? null, ['requested', 'received'], true);
    $label = match (true) {
        $stopping => __('stopping'),
        default => $job['state'],
    };
    $color = match (true) {
        $stopping => 'amber',
        $job['state'] === 'done' => 'green',
        $job['state'] === 'error' => 'red',
        $job['state'] === 'accepted' => 'blue',
        $job['state'] === 'waiting' => 'amber',
        default => 'zinc',
    };
@endphp

<flux:badge size="sm" :color="$color" data-test="job-state" data-state="{{ $job['state'] }}">{{ $label }}</flux:badge>
