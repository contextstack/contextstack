@props(['kind', 'figures'])

{{--
    One kind of source's figures from GET usage (CLIENT-API.md §12.4), each shown for what it is: pay as you go with
    the dollars actually charged; a subscription with its usage and the allowance its agent reported, never dollars;
    local with usage only. An API-price estimate is not a charge and is never shown.
--}}
@php
    $figures = is_array($figures) ? $figures : [];
    $tokens = collect(['input_tokens' => __('in'), 'output_tokens' => __('out'), 'cache_read_tokens' => __('cache read'),
        'cache_creation_tokens' => __('cache write'), 'reasoning_tokens' => __('reasoning')])
        ->filter(fn ($label, $field) => is_int($figures[$field] ?? null))
        ->map(fn ($label, $field) => number_format($figures[$field]).' '.$label)->implode(' / ');
    [$label, $color] = match ($kind) {
        'pay_as_you_go' => [__('pay as you go'), 'green'],
        'subscription' => [__('subscription'), 'blue'],
        'local' => [__('local'), 'zinc'],
        default => [__('unknown source'), 'zinc'],
    };
@endphp

<div class="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-sm" data-test="usage-kind" data-kind="{{ $kind }}">
    <flux:badge size="sm" :color="$color">{{ $label }}</flux:badge>
    <span>{{ trans_choice(':count run|:count runs', (int) ($figures['runs'] ?? 0)) }}</span>
    @if ($kind === 'pay_as_you_go')
        <span class="font-semibold" data-test="charged">{{ \App\Client\Money::dollars($figures['charged_usd'] ?? 0) }} {{ __('charged') }}</span>
        @if ((int) ($figures['charge_unreported_runs'] ?? 0) > 0)
            <span class="text-amber-600">{{ trans_choice(':count run reported no charge|:count runs reported no charge', (int) $figures['charge_unreported_runs']) }}</span>
        @endif
    @elseif ($kind === 'subscription')
        <span data-test="allowance">{{ $figures['shown'] ?? '' }}</span>
        @if (($figures['allowance_reached'] ?? false) === true)
            <flux:badge size="sm" color="red">{{ __('allowance reached') }}</flux:badge>
        @endif
        @if (is_string($figures['allowance']['reported_at'] ?? null))
            <span class="text-xs text-zinc-500">{{ __('reported :at', ['at' => $figures['allowance']['reported_at']]) }}</span>
        @endif
    @elseif ($kind === 'local')
        <span>{{ __('usage only, no cost') }}</span>
    @endif
    @if ($tokens !== '')
        <span class="text-zinc-500">{{ $tokens }}</span>
    @endif
    @if (($figures['reported_models'] ?? []) !== [])
        <span class="font-mono text-xs text-zinc-500">{{ implode(', ', (array) $figures['reported_models']) }}</span>
    @endif
</div>
