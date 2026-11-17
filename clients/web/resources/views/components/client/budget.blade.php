@props(['budget' => null])

{{--
    A project's or conversation's budget as the Director shows it (CLIENT-API.md §12.2). Each state looks different:
    "no budget set" is never shown as "budget used up". Dollars are actual pay-as-you-go charges, never estimates.
--}}
@php
    $budget = is_array($budget) ? $budget : ['state' => 'none', 'shown' => 'no budget set'];
    $state = (string) ($budget['state'] ?? 'none');
    [$label, $color, $icon] = match ($state) {
        'active' => [__('active'), 'green', 'check-circle'],
        'reserved' => [__('reserved'), 'amber', 'clock'],
        'exhausted' => [__('budget used up'), 'red', 'no-symbol'],
        default => [__('no budget set'), 'zinc', 'minus-circle'],
    };
    $money = fn ($value) => \App\Client\Money::dollars($value);
@endphp

<div {{ $attributes->merge(['class' => 'space-y-2']) }} data-test="budget" data-state="{{ $state }}">
    <div class="flex flex-wrap items-center gap-2">
        <flux:badge size="sm" :color="$color" :icon="$icon" data-test="budget-state">{{ $label }}</flux:badge>
        <flux:text data-test="budget-shown">{{ $budget['shown'] ?? $label }}</flux:text>
    </div>
    @if ($state !== 'none')
        <dl class="grid grid-cols-2 gap-2 text-sm sm:grid-cols-5">
            <div><dt class="text-zinc-500">{{ __('Budget') }}</dt><dd>{{ $money($budget['usd'] ?? null) }} {{ __('per') }} {{ $budget['period'] ?? 'total' }}</dd></div>
            <div><dt class="text-zinc-500">{{ __('Charged') }}</dt><dd>{{ $money($budget['spent_usd'] ?? null) }}</dd></div>
            <div><dt class="text-zinc-500">{{ __('Reserved for running work') }}</dt><dd>{{ $money($budget['reserved_usd'] ?? null) }}</dd></div>
            <div><dt class="text-zinc-500">{{ __('Left') }}</dt><dd>{{ $money($budget['remaining_usd'] ?? null) }}</dd></div>
            <div><dt class="text-zinc-500">{{ __('Resets') }}</dt><dd><x-client.time :at="$budget['resets_at']" :fallback="__('never (total)')" /></dd></div>
        </dl>
    @endif
</div>
