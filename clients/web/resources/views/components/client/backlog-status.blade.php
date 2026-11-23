@props(['status' => 'next'])

{{-- A backlog item's status as a small badge. --}}
@php($color = match ($status) { 'now' => 'blue', 'blocked' => 'amber', 'done' => 'green', 'dropped' => 'zinc', default => null })
@php($label = match ($status) { 'now' => __('now'), 'blocked' => __('blocked'), 'done' => __('done'), 'dropped' => __('dropped'), default => __('next') })
<flux:badge size="sm" :color="$color" data-test="backlog-status-badge" {{ $attributes }}>{{ $label }}</flux:badge>
