@use('App\Native\ReleaseApprovals')
{{-- The owner's controls on a release request besides approve and deny: acknowledge a rollback still waiting on jobs, and clear a stuck request after a warning and the device unlock. --}}
@if (($approval['status'] ?? null) === 'needs_owner' && $screen->mayDecideApproval($approval))
    <native:text class="text-xs text-theme-on-surface-variant">Approving or denying asks for Face ID, Touch ID or your passcode first.</native:text>
@endif

@if (ReleaseApprovals::acknowledgeable($approval) && \App\Native\DirectorScreen::action($approval, 'answer') !== false)
    <native:text class="text-sm text-theme-on-surface">Acknowledging takes this off your needs-you list. Intake stays frozen until the previous release is restored.</native:text>
    <native:button label="Acknowledge rollback is waiting" variant="secondary" @tap="acknowledgeRollback('{{ $approval['id'] }}')" />
@endif

@if ($screen->mayClear($approval))
    @if ($screen->confirmingClear === ($approval['id'] ?? null))
        <native:text ref="clear-confirm" class="text-sm text-theme-destructive">Clearing revokes this request's claim at the Director. A node worker that has already entered the installer may still finish, so check the live release on the node before asking for another switch.</native:text>
        <native:row class="w-full gap-3">
            <native:button label="Clear it" variant="destructive" @tap="confirmClear" />
            <native:button label="Cancel" variant="ghost" @tap="cancelClear" />
        </native:row>
    @else
        <native:button label="Clear stuck request" variant="ghost" icon="xmark.circle" @tap="askClear('{{ $approval['id'] }}')" />
    @endif
@endif
