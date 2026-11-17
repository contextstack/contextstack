@props(['back' => null, 'backLabel' => null])

{{--
    A detail page's header: where you came from as a small line, then the title with its state, and the page's
    actions on the right.
--}}
<header {{ $attributes->class('flex flex-wrap items-end gap-x-3 gap-y-2') }}>
    <div class="min-w-0 flex-1">
        @if ($back)
            <a href="{{ $back }}" wire:navigate class="inline-flex items-center gap-1 text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-white">
                <flux:icon.chevron-left variant="micro" class="-ms-0.5 size-3.5" />{{ $backLabel }}
            </a>
        @endif
        <div class="flex flex-wrap items-center gap-x-2.5 gap-y-1">
            <h1 class="min-w-0 text-lg font-semibold leading-7 tracking-tight">{{ $slot }}</h1>
            @isset($status)
                <div class="flex shrink-0 items-center gap-1">{{ $status }}</div>
            @endisset
        </div>
    </div>
    @isset($actions)
        <div class="flex shrink-0 items-center gap-2">{{ $actions }}</div>
    @endisset
</header>
