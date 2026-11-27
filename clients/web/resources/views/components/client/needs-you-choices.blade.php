@props(['needs', 'approval' => null])

{{--
    A needs-you question with options (docs/design/hosted-needs-you-client.md): each option is a button, the default
    is highlighted, and nothing is sent until one is pressed. Links open the matching page in the app. The host page
    supplies answerChoice(). A land approval shows its facts; only its owner answers it.
--}}
@php
    $answerable = \App\Client\NeedsYou::answerable($needs, is_array($approval) ? $approval : null);
    $isLand = ((is_array($approval) ? ($approval['kind'] ?? null) : null) ?? $needs['kind']) === 'land';
    $pinned = is_array($approval) && filled($approval['commit'] ?? null);
    // The item's own `actions.answer`, when sent, already says whether a land can be decided now.
    $decidedByDirector = is_array($needs['actions']) && array_key_exists('answer', $needs['actions']);
@endphp
<article class="space-y-3 rounded-xl border border-amber-500/30 bg-amber-500/[.06] px-4 py-3" data-test="needs-you-choices">
    <div class="flex items-center gap-2 text-xs text-zinc-600 dark:text-zinc-400">
        <flux:icon.bell variant="micro" class="text-amber-500" />
        <span class="font-medium text-zinc-800 dark:text-zinc-200" data-test="needs-you-assignment">{{ \App\Client\NeedsYou::assignment(['assignee_member_id' => $needs['assignee'], 'assignee_display_name' => $needs['assignee_name']]) }}</span>
        @if ($isLand)
            <flux:badge size="sm" data-test="land-badge">{{ __('Land on main') }}</flux:badge>
        @endif
    </div>
    @if ($needs['question'] !== '')
        <x-client.markdown :text="$needs['question']" class="!text-[15px] !leading-7" data-test="needs-you-question" />
    @elseif (is_array($approval) && filled($approval['reason'] ?? null))
        <p class="text-sm" data-test="needs-you-question">{{ $approval['reason'] }}</p>
    @endif

    @if ($isLand && is_array($approval))
        <x-client.land-facts :approval="$approval" />
    @endif

    @if ($needs['links'] !== [])
        <p class="flex flex-wrap gap-x-3 gap-y-1 text-xs" data-test="needs-you-links">
            @foreach ($needs['links'] as $kind => $id)
                @php($href = \App\Client\NeedsYou::href($kind, $id, $needs['project']))
                @if ($href !== null)
                    <flux:link :href="$href" wire:navigate class="font-mono" wire:key="needs-link-{{ $kind }}">{{ __(ucfirst($kind)) }}: {{ $id }}</flux:link>
                @endif
            @endforeach
        </p>
    @endif

    @if ($answerable && (! $isLand || $pinned || $decidedByDirector))
        <div class="flex flex-wrap gap-2" data-test="needs-you-options">
            @foreach ($needs['options'] as $option)
                <flux:button size="sm" :variant="$option['id'] === $needs['default'] ? 'primary' : 'outline'"
                             wire:click="answerChoice({{ Js::from($option['id']) }})" wire:key="option-{{ $option['id'] }}"
                             data-test="needs-you-option" data-option="{{ $option['id'] }}" :data-default="$option['id'] === $needs['default'] ? 'true' : null">
                    {{ $option['label'] }}
                </flux:button>
            @endforeach
        </div>
    @elseif ($isLand && ! $pinned && ! $decidedByDirector)
        <p class="text-xs text-zinc-500" data-test="land-not-pinned">{{ __('Waiting for its machine to pin the commit before it can be decided.') }}</p>
    @else
        <p class="text-xs text-zinc-500" data-test="needs-you-read-only">{{ __('You can read this; the person it is assigned to answers it.') }}</p>
    @endif
</article>
