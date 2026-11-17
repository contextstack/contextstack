@props(['usage' => null])

{{-- A run's usage as recorded (NODE-PROTOCOL.md §7.2). An API-price estimate (est_usd) is not a charge and is never shown. --}}
@if (is_array($usage))
    @php
        $source = is_array($usage['source'] ?? null) ? $usage['source'] : [];
        $class = $source['class'] ?? null;
    @endphp
    <span data-test="usage">
        {{ number_format((int) ($usage['input_tokens'] ?? 0)) }} in / {{ number_format((int) ($usage['output_tokens'] ?? 0)) }} out
        @if ($class === 'api')
            · {{ \App\Client\Money::dollars($usage['charged_usd'] ?? 0) }} {{ __('charged') }}
        @elseif ($class === 'subscription')
            · {{ __('subscription') }}
        @elseif ($class === 'free_local')
            · {{ __('local, no cost') }}
        @endif
        @if (filled($source['model'] ?? null))
            · <span class="font-mono">{{ $source['model'] }}</span>
        @endif
    </span>
@else
    <span class="text-zinc-500">{{ __('not reported') }}</span>
@endif
