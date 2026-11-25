@props(['approval', 'showJob' => true])

{{--
    One action approval (CLIENT-API.md "Action approvals"): the exact command an agent asked to run beyond its sandbox,
    what it needs, the agent's reason, the reviewer's verdict and reason, and, while it waits on the owner, Approve and
    Deny. The host page extends ApprovalsPage, which supplies decideApproval().
--}}
@php
    $id = (string) $approval['id'];
    $status = (string) ($approval['status'] ?? 'pending');
    $argv = array_values(array_filter((array) ($approval['argv'] ?? []), 'is_string'));
    $command = implode(' ', array_map(
        fn (string $arg): string => $arg !== '' && preg_match('/^[A-Za-z0-9_\/.,:=@%+-]+$/', $arg) ? $arg : "'".str_replace("'", "'\\''", $arg)."'",
        $argv,
    ));
    $needs = array_values(array_filter((array) ($approval['needs'] ?? []), 'is_string'));
    $paths = array_values(array_filter((array) ($approval['write_paths'] ?? []), 'is_string'));
    $verdict = $approval['verdict'] ?? null;
    $outcome = $approval['outcome']['status'] ?? null;
    $decidedByOwner = filled($approval['decided_by'] ?? null);
    [$label, $color] = match (true) {
        $status === 'pending' => [__('Under review'), 'zinc'],
        $status === 'needs_owner' => [__('Needs you'), 'amber'],
        $status === 'denied' => [__('Denied'), 'red'],
        $status === 'expired' => [__('Expired'), 'zinc'],
        $status === 'approved' && $decidedByOwner => [__('Approved by you'), 'green'],
        $status === 'approved' => [__('Approved by the reviewer'), 'green'],
        default => [$status, 'zinc'],
    };
    $waiting = $status === 'needs_owner';
@endphp

<article @class([
    'space-y-3 rounded-xl border px-4 py-3',
    'border-amber-500/30 bg-amber-500/[.06]' => $waiting,
    'border-black/[.08] dark:border-white/[.08]' => ! $waiting,
]) data-test="action-approval" data-approval="{{ $id }}" data-status="{{ $status }}" wire:key="approval-{{ $id }}">
    <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-600 dark:text-zinc-400">
        <flux:icon.shield-exclamation variant="micro" @class(['text-amber-500' => $waiting]) />
        <span class="font-medium text-zinc-800 dark:text-zinc-200">{{ __('An agent asks to go beyond its sandbox') }}</span>
        <flux:badge size="sm" :color="$color" data-test="approval-status">{{ $label }}</flux:badge>
        @if (filled($approval['created_at'] ?? null))
            <span>· <x-client.time :at="$approval['created_at']" /></span>
        @endif
    </div>

    <pre class="overflow-x-auto whitespace-pre-wrap break-all rounded-md bg-black/5 px-3 py-2 font-mono text-[13px] dark:bg-white/[.07]" data-test="approval-command">{{ $command }}</pre>

    <dl class="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[8rem_minmax(0,1fr)]">
        @if ($needs !== [])
            <dt class="text-zinc-500">{{ __('Needs') }}</dt>
            <dd class="flex flex-wrap gap-1" data-test="approval-needs">
                @foreach ($needs as $need)
                    <flux:badge size="sm">{{ $need }}</flux:badge>
                @endforeach
            </dd>
        @endif
        @if ($paths !== [])
            <dt class="text-zinc-500">{{ __('Writes to') }}</dt>
            <dd class="space-y-0.5 font-mono text-xs" data-test="approval-paths">
                @foreach ($paths as $path)
                    <div class="break-all">{{ $path }}</div>
                @endforeach
            </dd>
        @endif

        <dt class="text-zinc-500">{{ __('Agent\'s reason') }}</dt>
        <dd data-test="approval-reason">
            {{ $approval['reason'] ?? '' }}
            @if (filled($approval['requesting_model'] ?? null))
                <span class="text-xs text-zinc-500">· {{ $approval['requesting_model'] }}</span>
            @endif
        </dd>

        <dt class="text-zinc-500">{{ __('Reviewer') }}</dt>
        <dd data-test="approval-review">
            @if ($status === 'pending')
                <span class="text-zinc-500">{{ __('A second model is reviewing the request…') }}</span>
            @else
                @if ($verdict !== null)
                    <flux:badge size="sm" :color="$verdict === 'go' ? 'green' : 'red'" data-test="approval-verdict">{{ $verdict === 'go' ? __('go') : __('no-go') }}</flux:badge>
                @endif
                {{ $approval['review_reason'] ?? '' }}
                <span class="text-xs text-zinc-500">· {{ filled($approval['reviewer_model'] ?? null) ? $approval['reviewer_model'] : __('no reviewer model was selected or called') }}</span>
            @endif
        </dd>

        @if ($showJob && filled($approval['job_id'] ?? null))
            <dt class="text-zinc-500">{{ __('Task lane') }}</dt>
            <dd><flux:link :href="route('lanes.show', $approval['job_id'])" wire:navigate class="font-mono text-xs" data-test="approval-job">{{ $approval['job_id'] }}</flux:link></dd>
        @endif
    </dl>

    @if ($waiting)
        <div class="flex flex-wrap items-center gap-2">
            <flux:button size="sm" variant="primary" icon="check" wire:click="decideApproval(@js($id), 'approve')" data-test="approve-action">{{ __('Approve') }}</flux:button>
            <flux:button size="sm" variant="danger" icon="x-mark" wire:click="decideApproval(@js($id), 'deny')" data-test="deny-action">{{ __('Deny') }}</flux:button>
            <span class="text-xs text-zinc-500">{{ __('Approving lets the agent run this once, with only what it asked for.') }}</span>
        </div>
    @else
        <p class="text-xs text-zinc-500" data-test="approval-outcome">
            @if ($outcome === 'succeeded')
                {{ __('It ran and succeeded.') }}
            @elseif ($outcome === 'failed')
                {{ __('It ran and failed') }}@if (isset($approval['outcome']['exit_code'])) {{ __('(exit :code)', ['code' => $approval['outcome']['exit_code']]) }}@endif.
            @elseif ($status === 'approved' && filled($approval['claimed_at'] ?? null))
                {{ __('The agent took the approval; it may have run, and no result was recorded.') }}
            @elseif ($status === 'approved')
                {{ __('Approved. It runs if the agent asks again while its run is still going.') }}
            @elseif ($status === 'denied')
                {{ __('It did not run.') }}
            @elseif ($status === 'expired')
                {{ __('Its job ended before it was decided, so it did not run.') }}
            @endif
            @if ($decidedByOwner && filled($approval['decided_at'] ?? null))
                {{ __('Decided') }} <x-client.time :at="$approval['decided_at']" />.
            @endif
        </p>
    @endif
</article>
