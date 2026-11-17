@props(['thread', 'indent' => false])

{{-- One conversation in the sidebar list: its name, a dot when it needs you or is working, a wrench when it runs tasks. --}}
<a href="{{ route('conversations.show', $thread['id']) }}" wire:navigate.hover wire:key="rail-{{ $thread['id'] }}"
   wire:current="cs-thread-current"
   @class([
       'flex h-8 items-center gap-2 rounded-lg px-2 text-sm text-zinc-700 hover:bg-black/5 dark:text-zinc-300 dark:hover:bg-white/[.06] [&.cs-thread-current]:bg-black/[.07] [&.cs-thread-current]:text-zinc-950 dark:[&.cs-thread-current]:bg-white/10 dark:[&.cs-thread-current]:text-white',
       'ps-7' => $indent,
   ])
   data-test="rail-thread" data-thread="{{ $thread['id'] }}" {{ $attributes }}>
    <span class="min-w-0 flex-1 truncate">{{ $thread['name'] }}</span>
    @if ($thread['needs_you'])
        <span class="size-1.5 shrink-0 rounded-full bg-amber-500" title="{{ __('Needs you') }}" data-test="rail-needs-you"></span>
    @elseif ($thread['running'])
        <span class="size-1.5 shrink-0 animate-pulse rounded-full bg-blue-500" title="{{ __('Working') }}" data-test="rail-running"></span>
    @endif
    @if ($thread['lead'])
        <flux:icon.wrench-screwdriver variant="micro" class="size-3.5 shrink-0 text-zinc-400" title="{{ __('Runs tasks') }}" data-test="rail-lead" />
    @endif
</a>
