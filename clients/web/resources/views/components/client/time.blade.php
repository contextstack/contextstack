@props(['at' => null, 'fallback' => ''])

{{-- A Director time (UTC ISO 8601), shown in the viewer's time zone by resources/js/local-times.js; the UTC value stays in the tooltip. --}}
@if (is_string($at) && $at !== '')
<time datetime="{{ $at }}" title="{{ $at }}" data-local {{ $attributes->class("whitespace-nowrap") }}>{{ $at }}</time>@else{{ $fallback }}@endif
