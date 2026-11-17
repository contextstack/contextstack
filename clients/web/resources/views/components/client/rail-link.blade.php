@props(['href', 'icon', 'label', 'exact' => false])

{{-- One icon in the thin left rail: a 36px target, the label as a tooltip, lit while its section is open. --}}
<flux:tooltip :content="$label" position="right">
    <a href="{{ $href }}" wire:navigate.hover @if ($exact) wire:current.exact="cs-rail-current" @else wire:current="cs-rail-current" @endif
       aria-label="{{ $label }}"
       {{ $attributes->class('flex size-9 items-center justify-center rounded-lg text-zinc-500 transition-colors hover:bg-black/5 hover:text-zinc-900 dark:text-zinc-400 dark:hover:bg-white/[.07] dark:hover:text-white [&.cs-rail-current]:bg-black/[.07] [&.cs-rail-current]:text-zinc-900 dark:[&.cs-rail-current]:bg-white/10 dark:[&.cs-rail-current]:text-white') }}>
        <flux:icon :name="$icon" class="size-5" />
    </a>
</flux:tooltip>
