@props(['heading' => null])

{{-- A titled block of a page: a quiet heading and its content, no card around it. --}}
<section {{ $attributes->class('space-y-2') }}>
    @if ($heading)
        <h2 class="text-sm font-medium text-zinc-500">{{ $heading }}</h2>
    @endif
    {{ $slot }}
</section>
