@props(['jobs' => []])

{{-- Jobs held by a budget (CLIENT-API.md §12.3): created, not offered, with the Director's reason in its own words. --}}
@if ($jobs !== [])
    <div class="space-y-2" data-test="waiting-jobs">
        <flux:heading size="sm">{{ trans_choice(':count job waits|:count jobs wait', count($jobs)) }}</flux:heading>
        @foreach ($jobs as $job)
            <div class="flex flex-wrap items-baseline gap-2 text-sm" wire:key="waiting-{{ $job['job_id'] }}" data-test="waiting-job" data-reason="{{ $job['budget']['reason'] ?? '' }}">
                @if (is_string($job['intent_id'] ?? null))
                    <flux:link :href="route('intents.show', $job['intent_id'])" wire:navigate class="font-mono">{{ $job['job_id'] }}</flux:link>
                @elseif (is_string($job['conversation_id'] ?? null))
                    <flux:link :href="route('conversations.show', $job['conversation_id'])" wire:navigate class="font-mono">{{ $job['job_id'] }}</flux:link>
                @else
                    <span class="font-mono">{{ $job['job_id'] }}</span>
                @endif
                <flux:badge size="sm" color="amber">{{ match ($job['budget']['reason'] ?? null) { 'budget_exhausted' => __('budget used up'), 'budget_reserved' => __('room reserved for running work'), default => __('waiting') } }}</flux:badge>
                <flux:text>{{ $job['budget']['detail'] ?? '' }}</flux:text>
            </div>
        @endforeach
    </div>
@endif
