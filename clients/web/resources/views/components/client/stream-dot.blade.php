{{-- The live stream's state as a dot (green when live); needs a directorStream() scope around it. --}}
<span {{ $attributes->class('inline-flex items-center') }} x-bind:title="status" data-test="stream-status">
    <span class="size-1.5 rounded-full bg-zinc-400" x-bind:class="status === 'live' ? '!bg-emerald-500' : 'animate-pulse'"></span>
    <span class="sr-only" x-text="status">connecting</span>
</span>
