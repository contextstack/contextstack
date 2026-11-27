@use('App\Native\ReleaseApprovals')
{{-- A hosted install or rollback: what the node inspected and pinned, what approving covers, and what happened after. --}}
@php($outcome = ReleaseApprovals::outcome($approval))
<native:text ref="release-status" class="text-sm font-bold text-theme-on-surface">{{ ReleaseApprovals::status($approval) }}</native:text>
@if (($approval['kind'] ?? null) === 'install')
    <native:text class="text-sm font-bold text-theme-on-surface">Release</native:text>
    <native:text ref="release-source" class="font-mono text-base text-theme-on-surface">{{ ReleaseApprovals::source($approval) }}</native:text>
@endif

@if (($approval['status'] ?? null) === 'pending')
    <native:text class="text-sm text-theme-on-surface-variant">The node is inspecting the request and pinning the exact commit. Nothing can be decided until it has.</native:text>
@endif

@if (ReleaseApprovals::pinned($approval))
    <native:text class="text-sm font-bold text-theme-on-surface">Commit</native:text>
    <native:text ref="release-commit" class="font-mono text-sm text-theme-on-surface">{{ $approval['commit'] }}</native:text>
    @if ((string) ($approval['commit_provenance'] ?? '') !== '')
        <native:text class="text-xs text-theme-on-surface-variant">{{ $approval['commit_provenance'] }}</native:text>
    @endif
    <native:text class="text-sm font-bold text-theme-on-surface">Target hash</native:text>
    <native:text ref="release-hash" class="font-mono text-sm text-theme-on-surface">{{ $approval['target_hash'] }}</native:text>
    <native:text class="text-sm font-bold text-theme-on-surface">Schema</native:text>
    <native:text ref="release-schema" class="text-sm text-theme-on-surface">Current: {{ ReleaseApprovals::schema($approval['schema_current'] ?? null) }} · Target: {{ ReleaseApprovals::schema($approval['schema_target'] ?? null) }}</native:text>
    @if ((string) ($approval['backup'] ?? '') !== '')
        <native:text class="text-xs text-theme-on-surface-variant">Backup: {{ $approval['backup'] }}</native:text>
    @endif
    <native:text class="text-sm font-bold text-theme-on-surface">Release notes diff</native:text>
    <native:text ref="release-notes" class="font-mono text-xs text-theme-on-surface">{{ (string) ($approval['release_notes_diff'] ?? '') !== '' ? $approval['release_notes_diff'] : '(no changes to the release notes)' }}</native:text>
@endif

@if (($approval['status'] ?? null) === 'needs_owner')
    @if (($approval['kind'] ?? null) === 'install')
        <native:text ref="release-covers" class="text-sm text-theme-on-surface">Approving this install also approves restoring the previous release automatically if the smoke trial fails.</native:text>
    @else
        <native:text ref="release-covers" class="text-sm text-theme-on-surface">Approving rolls the node back to its previous installed release, after the installer's usual schema compatibility check.</native:text>
    @endif
@endif

@if (ReleaseApprovals::outcomeWords($approval) !== '')
    <native:text ref="release-outcome" class="text-base text-theme-on-surface">{{ ReleaseApprovals::outcomeWords($approval) }}</native:text>
@endif
@if ((string) ($outcome['release'] ?? '') !== '')
    <native:text ref="release-current" class="text-sm text-theme-on-surface">Current release: {{ $outcome['release'] }}</native:text>
@endif
@if (ReleaseApprovals::smoke($outcome) !== '')
    <native:text ref="release-smoke" class="text-sm font-bold {{ ($outcome['smoke_ok'] ?? null) === true ? 'text-theme-on-surface' : 'text-theme-destructive' }}">{{ ReleaseApprovals::smoke($outcome) }}</native:text>
    @foreach (ReleaseApprovals::checks($outcome) as $check)
        <native:text native:key="check-{{ $loop->index }}" class="font-mono text-xs text-theme-on-surface">{{ $check }}</native:text>
    @endforeach
@endif
@if ((string) ($outcome['reason'] ?? '') !== '' && ! in_array($outcome['status'] ?? null, ['waiting_on_owner', 'unknown'], true))
    <native:text ref="release-reason" class="text-sm text-theme-on-surface-variant">{{ $outcome['reason'] }}</native:text>
@endif
@if (($outcome['switched_without_smoke'] ?? false) === true)
    <native:text class="text-sm text-theme-destructive">It may have switched releases without a smoke trial. Check the live release on the node.</native:text>
@endif

@if ($lastSmoke !== [])
    <native:text class="text-sm font-bold text-theme-on-surface">Latest smoke on this machine</native:text>
    <native:text ref="last-smoke" class="text-sm text-theme-on-surface">{{ ReleaseApprovals::smoke($lastSmoke) }}{{ (string) ($lastSmoke['release'] ?? '') !== '' ? ' · release '.$lastSmoke['release'] : '' }}</native:text>
    @if ((string) ($lastSmoke['reason'] ?? '') !== '')
        <native:text class="text-xs text-theme-on-surface-variant">{{ $lastSmoke['reason'] }}</native:text>
    @endif
@endif

<native:text class="text-xs text-theme-on-surface-variant">Conversation {{ (string) ($approval['conversation_id'] ?? '—') }}. Its thread gets a post-install turn with the same summary.</native:text>
