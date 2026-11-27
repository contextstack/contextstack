@props(['conversation', 'compact' => false])

{{-- Conversation attention (CLIENT-API.md §11.7): a lead or owner asks for one decision; cleared on dismiss or owner reply. --}}
@php
    $attention = $conversation['attention'] ?? null;
    $active = is_array($attention) && filled($attention['question'] ?? null);
@endphp

@if ($active)
    <div @class(['flex flex-col gap-1' => $compact]) data-test="conversation-needs-you">
        <flux:badge size="sm" color="amber" icon="bell" data-test="needs-you-badge">{{ \App\Client\NeedsYou::assignment($attention) }}</flux:badge>
        @if (($attention['source_kind'] ?? null) === 'lead')
            <span data-test="needs-you-source">{{ __('asked by the agent in :conversation', ['conversation' => $conversation['conversation_id']]) }}</span>
        @endif
        @if ($compact)
            <x-client.markdown :text="$attention['question']" class="line-clamp-1 text-zinc-600 dark:text-zinc-400" data-test="needs-you-question" />
        @endif
    </div>
@endif
