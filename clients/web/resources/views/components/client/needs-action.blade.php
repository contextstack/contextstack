@props(['heading'])

{{-- The one block on a page that waits on you: tinted so it reads first, not boxed like everything else. --}}
<section {{ $attributes->class('space-y-3 rounded-xl border border-amber-500/30 bg-amber-500/[.06] px-4 py-3') }}>
    <h2 class="text-sm font-medium">{{ $heading }}</h2>
    {{ $slot }}
</section>
