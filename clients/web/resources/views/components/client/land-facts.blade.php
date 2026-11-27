@props(['approval'])

{{--
    A hosted land approval's facts (CLIENT-API.md "Hosted land approvals", docs/design/hosted-land-client.md): the
    repository, the lane, the full pinned commit, the review reason, and both path flags said outright.
--}}
@php
    $flag = fn (mixed $value): string => match ($value) { true => __('yes'), false => __('no'), default => __('not inspected yet') };
@endphp
<dl class="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[10rem_minmax(0,1fr)]" data-test="land-facts">
    <dt class="text-zinc-500">{{ __('Repository') }}</dt>
    <dd class="font-mono text-xs" data-test="land-repo">{{ $approval['repo'] ?? '' }}</dd>
    <dt class="text-zinc-500">{{ __('Lane') }}</dt>
    <dd class="font-mono text-xs" data-test="land-lane">{{ $approval['lane'] ?? '' }}</dd>
    <dt class="text-zinc-500">{{ __('Commit') }}</dt>
    <dd class="break-all font-mono text-xs" data-test="land-commit">{{ filled($approval['commit'] ?? null) ? $approval['commit'] : __('not pinned yet') }}</dd>
    @if (filled($approval['base'] ?? null))
        <dt class="text-zinc-500">{{ __('Onto') }}</dt>
        <dd class="break-all font-mono text-xs" data-test="land-base">{{ $approval['base'] }}</dd>
    @endif
    <dt class="text-zinc-500">{{ __('Protected paths changed') }}</dt>
    <dd data-test="land-protected">{{ $flag($approval['protected'] ?? null) }}</dd>
    <dt class="text-zinc-500">{{ __('Changes tracing/') }}</dt>
    <dd data-test="land-tracing">{{ $flag($approval['tracing'] ?? null) }}</dd>
    @if (filled($approval['review_reason'] ?? null))
        <dt class="text-zinc-500">{{ __('Review') }}</dt>
        <dd data-test="land-review">{{ $approval['review_reason'] }}</dd>
    @endif
</dl>
