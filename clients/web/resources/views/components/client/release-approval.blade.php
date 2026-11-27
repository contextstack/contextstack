@props(['approval', 'needsYou' => false, 'showConversation' => true])

{{--
    A hosted release approval (CLIENT-API.md "Hosted release approvals", docs/design/hosted-release-client.md): an
    owner-only install or rollback of the node's release. While the node inspects: the bundle (or local main) and the
    inspection status. On needs_owner: the full commit, the target hash, current and target schema, and the release
    notes diff as text, with Approve and Deny. After approval: a waiting state until the node reports, then the
    current release, the smoke result and the reason. An owner may clear a stuck request. The host page extends
    ApprovalsPage (decideApproval, clearApproval, acknowledgeRelease).
--}}
@php
    use App\Client\ReleaseApproval;

    $id = (string) $approval['id'];
    $isInstall = ($approval['kind'] ?? null) === 'install';
    $phase = ReleaseApproval::phase($approval);
    $outcome = is_array($approval['outcome'] ?? null) ? $approval['outcome'] : [];
    $outcomeStatus = ReleaseApproval::outcomeStatus($approval);
    $bundle = is_string($approval['bundle'] ?? null) && $approval['bundle'] !== '' ? $approval['bundle'] : null;
    $schema = fn (mixed $value): string => is_int($value) ? (string) $value : __('No local Director');
    $waitingRollback = ReleaseApproval::rollbackWaiting($approval);
    $cleared = ($outcome['cleared'] ?? false) === true;
    [$label, $color] = match ($phase) {
        'inspecting' => [__('Node inspecting'), 'zinc'],
        'needs_owner' => [\App\Client\NeedsYou::assignment($approval), 'amber'],
        'waiting' => [filled($approval['claimed_at'] ?? null) ? __('Switching') : __('Approved; waiting for the node'), 'sky'],
        'reported' => ReleaseApproval::outcomeLabel($outcomeStatus),
        'expired' => [__('Expired'), 'zinc'],
        default => [$cleared ? __('Cleared') : __('Denied'), 'zinc'],
    };
    $highlight = $phase === 'needs_owner' || $waitingRollback || $needsYou;
    $decidable = ReleaseApproval::decidable($approval);
    $clearable = ReleaseApproval::clearable($approval);
    $acknowledgeable = ReleaseApproval::acknowledgeable($approval);
    // Each check is {name, ok, reason?}; one without a string name or a boolean ok is malformed and skipped.
    $checks = array_values(array_filter((array) ($outcome['smoke']['checks'] ?? []),
        fn (mixed $check): bool => is_array($check) && is_string($check['name'] ?? null) && $check['name'] !== '' && is_bool($check['ok'] ?? null)));
@endphp

<article id="release-{{ $id }}" @class([
    'space-y-3 rounded-xl border px-4 py-3',
    'border-amber-500/30 bg-amber-500/[.06]' => $highlight,
    'border-black/[.08] dark:border-white/[.08]' => ! $highlight,
]) data-test="release-approval" data-approval="{{ $id }}" data-kind="{{ $approval['kind'] }}" data-phase="{{ $phase }}" wire:key="release-{{ $id }}">
    <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-600 dark:text-zinc-400">
        <flux:icon.rocket-launch variant="micro" @class(['text-amber-500' => $highlight]) />
        <span class="font-medium text-zinc-800 dark:text-zinc-200">{{ $isInstall ? __('Install a release on this node') : __('Roll this node back to its previous release') }}</span>
        @if ($needsYou && $phase !== 'needs_owner')
            <flux:badge size="sm" color="amber" icon="bell" data-test="release-needs-you">{{ \App\Client\NeedsYou::assignment($approval) }}</flux:badge>
        @endif
        <flux:badge size="sm" :color="$color" data-test="release-status">{{ $label }}</flux:badge>
        @if (filled($approval['created_at'] ?? null))
            <span>· <x-client.time :at="$approval['created_at']" /></span>
        @endif
        @if ($showConversation && filled($approval['conversation_id'] ?? null))
            <span>·</span>
            <flux:link :href="route('conversations.show', $approval['conversation_id'])" wire:navigate class="font-mono" data-test="release-conversation">{{ $approval['conversation_id'] }}</flux:link>
        @endif
    </div>

    <dl class="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[9rem_minmax(0,1fr)]" data-test="release-facts">
        @if ($isInstall)
            <dt class="text-zinc-500">{{ __('Source') }}</dt>
            <dd data-test="release-source">
                @if ($bundle !== null)
                    {{ __('Bundle') }} <span class="font-mono text-xs">{{ $bundle }}</span>
                @else
                    {{ __('The node\'s local main') }}
                @endif
                @if (filled($approval['commit_provenance'] ?? null))
                    <span class="text-xs text-zinc-500">· {{ $approval['commit_provenance'] }}</span>
                @endif
            </dd>
        @endif

        @if ($phase === 'inspecting')
            <dt class="text-zinc-500">{{ __('Inspection') }}</dt>
            <dd class="text-zinc-500" data-test="release-inspecting">{{ __('The node is inspecting the request and pinning the release…') }}</dd>
        @endif

        @if (filled($approval['commit'] ?? null))
            <dt class="text-zinc-500">{{ __('Commit') }}</dt>
            <dd class="break-all font-mono text-xs" data-test="release-commit">{{ $approval['commit'] }}</dd>
        @endif
        @if (filled($approval['target_hash'] ?? null))
            <dt class="text-zinc-500">{{ __('Target hash') }}</dt>
            <dd class="break-all font-mono text-xs" data-test="release-target-hash">{{ $approval['target_hash'] }}</dd>
        @endif
        @if (filled($approval['commit'] ?? null) || filled($approval['target_hash'] ?? null))
            <dt class="text-zinc-500">{{ __('Director schema') }}</dt>
            <dd data-test="release-schema">
                <span data-test="release-schema-current">{{ $schema($approval['schema_current'] ?? null) }}</span>
                <span class="text-zinc-500">→</span>
                <span data-test="release-schema-target">{{ $schema($approval['schema_target'] ?? null) }}</span>
            </dd>
        @endif
        @if (filled($approval['backup'] ?? null))
            <dt class="text-zinc-500">{{ __('Backup') }}</dt>
            <dd data-test="release-backup">{{ $approval['backup'] }}</dd>
        @endif
        @if (filled($approval['review_reason'] ?? null) && $phase !== 'reported')
            <dt class="text-zinc-500">{{ __('Node') }}</dt>
            <dd data-test="release-review">{{ $approval['review_reason'] }}</dd>
        @endif
    </dl>

    @if (is_string($approval['release_notes_diff'] ?? null))
        <details @if ($phase === 'needs_owner') open @endif>
            <summary class="cursor-pointer text-sm text-zinc-600 dark:text-zinc-300">{{ __('Release notes diff') }}</summary>
            <pre class="mt-2 max-h-96 overflow-auto whitespace-pre-wrap break-words rounded-md bg-black/5 px-3 py-2 font-mono text-[12px] dark:bg-white/[.07]" data-test="release-notes-diff">{{ $approval['release_notes_diff'] !== '' ? $approval['release_notes_diff'] : __('(no changes to the release notes)') }}</pre>
        </details>
    @endif

    @if ($phase === 'needs_owner')
        @if ($decidable)
            <div class="flex flex-wrap items-center gap-2">
                <flux:button size="sm" variant="primary" icon="check" wire:click="decideApproval(@js($id), 'approve')" data-test="approve-release">{{ $isInstall ? __('Approve install') : __('Approve rollback') }}</flux:button>
                <flux:button size="sm" variant="danger" icon="x-mark" wire:click="decideApproval(@js($id), 'deny')" data-test="deny-release">{{ __('Deny') }}</flux:button>
            </div>
        @else
            <p class="text-xs text-zinc-500" data-test="release-read-only">{{ __('Only an owner approves a release switch.') }}</p>
        @endif
        <p class="text-xs text-zinc-500" data-test="release-approval-covers">
            @if ($isInstall)
                {{ __('Approving installs exactly this pin and runs a smoke trial. If the trial fails, the node restores the previous release automatically; that restore is part of this approval.') }}
            @else
                {{ __('Approving restores the previous installed release, after the installer\'s usual schema compatibility check.') }}
            @endif
        </p>
    @elseif ($phase === 'waiting')
        <p class="flex items-center gap-2 text-sm text-zinc-600 dark:text-zinc-300" data-test="release-waiting">
            <flux:icon.clock variant="micro" />
            {{ filled($approval['claimed_at'] ?? null)
                ? __('The node took the request and is switching releases. The result shows here when it reports.')
                : __('Approved. Waiting for the node to pick it up and report.') }}
        </p>
    @elseif ($phase === 'reported')
        <dl class="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[9rem_minmax(0,1fr)]" data-test="release-outcome">
            @if (filled($outcome['release'] ?? null))
                <dt class="text-zinc-500">{{ __('Current release') }}</dt>
                <dd class="font-mono text-xs" data-test="release-current">{{ $outcome['release'] }}</dd>
            @endif
            @if (array_key_exists('smoke_ok', $outcome))
                <dt class="text-zinc-500">{{ __('Smoke') }}</dt>
                <dd data-test="release-smoke">
                    <flux:badge size="sm" :color="$outcome['smoke_ok'] === true ? 'green' : 'red'">{{ $outcome['smoke_ok'] === true ? __('passed') : __('failed') }}</flux:badge>
                </dd>
            @endif
            @if (filled($outcome['reason'] ?? null))
                <dt class="text-zinc-500">{{ __('Reason') }}</dt>
                <dd data-test="release-reason">{{ $outcome['reason'] }}</dd>
            @endif
            @if (is_int($outcome['waiting_jobs'] ?? null))
                <dt class="text-zinc-500">{{ __('Jobs left') }}</dt>
                <dd data-test="release-waiting-jobs">{{ trans_choice(':count job still running|:count jobs still running', $outcome['waiting_jobs'], ['count' => $outcome['waiting_jobs']]) }}</dd>
            @endif
        </dl>
        @if ($checks !== [])
            <ul class="space-y-0.5 text-xs" data-test="release-smoke-checks">
                @foreach ($checks as $check)
                    <li class="flex flex-wrap items-baseline gap-x-2" data-test="release-smoke-check" data-ok="{{ $check['ok'] ? 'true' : 'false' }}">
                        <span @class(['text-emerald-600' => $check['ok'], 'text-red-600' => ! $check['ok']])>{{ $check['ok'] ? '✓' : '✗' }}</span>
                        <span class="font-mono">{{ $check['name'] }}</span>
                        @if (is_string($check['reason'] ?? null) && $check['reason'] !== '')
                            <span class="text-zinc-500">{{ $check['reason'] }}</span>
                        @endif
                    </li>
                @endforeach
            </ul>
        @endif
        @if ($outcomeStatus === 'interrupted' && ($outcome['switched_without_smoke'] ?? false) === true)
            <p class="text-xs text-red-600 dark:text-red-400" data-test="release-switched-without-smoke">{{ __('The node switched releases before it could run the smoke trial. Check the live release on the node.') }}</p>
        @endif
        @if ($waitingRollback)
            <p class="text-xs text-zinc-600 dark:text-zinc-300" data-test="release-rollback-waiting">{{ __('The failed release is live and intake is frozen. The node restores the previous release once the running jobs finish. Acknowledging leaves intake frozen; restoring the previous release lifts the hold. If the live database is newer than the previous release supports, restore the matching snapshot as part of recovery.') }}</p>
            @if ($acknowledgeable)
                <flux:button size="sm" variant="primary" wire:click="acknowledgeRelease(@js($id))" data-test="acknowledge-release">{{ __('Acknowledge rollback is waiting') }}</flux:button>
            @endif
        @endif
    @elseif ($phase === 'expired' || $phase === 'denied')
        <p class="text-xs text-zinc-500" data-test="release-ended">
            {{ $outcome['reason'] ?? ($phase === 'expired' ? __('The request expired before it was decided; nothing was switched.') : __('Nothing was switched.')) }}
        </p>
    @endif

    @if ($clearable)
        <div class="flex justify-end">
            <flux:modal.trigger :name="'clear-release-'.$id">
                <flux:button size="xs" variant="ghost" icon="x-circle" data-test="clear-release">{{ __('Clear stuck request') }}</flux:button>
            </flux:modal.trigger>
        </div>
        <flux:modal :name="'clear-release-'.$id" class="max-w-md" data-test="clear-release-modal">
            <div class="space-y-4">
                <flux:heading size="lg">{{ __('Clear this release request?') }}</flux:heading>
                <flux:text>{{ __('Clearing revokes the node\'s claim at the Director and ends the request. A node worker that already entered the installer may still finish, so check the live release on the node before you ask again.') }}</flux:text>
                <div class="flex justify-end gap-2">
                    <flux:modal.close>
                        <flux:button variant="ghost">{{ __('Keep it') }}</flux:button>
                    </flux:modal.close>
                    <flux:button variant="danger" wire:click="clearApproval(@js($id))" data-test="confirm-clear-release">{{ __('Clear request') }}</flux:button>
                </div>
            </div>
        </flux:modal>
    @endif
</article>
