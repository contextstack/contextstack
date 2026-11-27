@props(['smoke'])

{{--
    The node's last smoke trial (CLIENT-API.md "Hosted release approvals", `GET conversations/{id}/smoke`): the
    current release, whether the smoke passed, and the outcome reason, from the newest install or rollback that ran
    one on this conversation's node.
--}}
@php
    [$label, $color] = \App\Client\ReleaseApproval::outcomeLabel(is_string($smoke['status'] ?? null) ? $smoke['status'] : null);
@endphp
<div class="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-lg border border-black/[.06] px-3 py-2 text-xs text-zinc-600 dark:border-white/[.06] dark:text-zinc-400" data-test="release-last-smoke">
    <span class="font-medium text-zinc-800 dark:text-zinc-200">{{ __('Last smoke on this node') }}</span>
    @if (array_key_exists('smoke_ok', $smoke))
        <flux:badge size="sm" :color="$smoke['smoke_ok'] === true ? 'green' : 'red'" data-test="last-smoke-result">{{ $smoke['smoke_ok'] === true ? __('passed') : __('failed') }}</flux:badge>
    @endif
    @if (filled($smoke['status'] ?? null))
        <flux:badge size="sm" :color="$color" data-test="last-smoke-status">{{ $label }}</flux:badge>
    @endif
    @if (filled($smoke['release'] ?? null))
        <span>{{ __('Current release') }} <span class="font-mono" data-test="last-smoke-release">{{ $smoke['release'] }}</span></span>
    @endif
    @if (filled($smoke['reason'] ?? null))
        <span data-test="last-smoke-reason">· {{ $smoke['reason'] }}</span>
    @endif
</div>
