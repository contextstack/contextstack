@use('App\Native\ActionApprovals')
@use('App\Native\BrowserActions')
@use('App\Native\ReleaseApprovals')

@if ($linked)
    {{-- Pushed from a needs-you link: a back button returns to where it was opened. --}}
    <native:top-bar title="Approval" back="true">
        <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
    </native:top-bar>
@else
    <native:top-bar title="Waiting on you" subtitle="{{ $loaded ? ($count === 0 ? 'Nothing waits on you' : $count.' waiting') : 'Loading' }}">
        <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
    </native:top-bar>
@endif

@include('native.partials.tabs', ['waitingCount' => $count])

<native:row class="w-full h-full bg-theme-background">
    {{-- The list: always on an iPad; on a phone only while nothing is selected. --}}
    <native:scroll-view class="{{ $selectedApproval ? 'hidden md:flex' : 'flex' }} h-full w-full md:w-96">
        <native:column class="w-full gap-2 p-4">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($backlogProblem !== '')
                <native:text class="text-sm text-theme-on-surface-variant">{{ $backlogProblem }}</native:text>
            @endif

            @if ($loaded && $count === 0)
                <native:text class="text-base text-theme-on-surface-variant">Nothing waits on you.</native:text>
            @endif

            @foreach ($approvals as $id => $approval)
                <native:list-item native:key="approval-{{ $id }}"
                    headline="{{ ActionApprovals::headline($approval) }}"
                    supporting="{{ (string) ($approval['reason'] ?? '') }}"
                    overline="{{ ReleaseApprovals::isRelease($approval) ? 'Release approval' : (ActionApprovals::isLand($approval) ? 'Land approval' : (BrowserActions::is($approval) ? 'Browser action' : 'Action approval')) }} · {{ $this->assignment($approval) }}"
                    leadingIcon="{{ ReleaseApprovals::isRelease($approval) ? 'shippingbox' : (ActionApprovals::isLand($approval) ? 'arrow.triangle.merge' : (BrowserActions::is($approval) ? 'globe' : 'shield')) }}"
                    @press="select('{{ $id }}')" />
            @endforeach

            @foreach ($backlog as $waiting)
                <native:list-item native:key="backlog-{{ $waiting['item_id'] }}"
                    headline="{{ $waiting['title'] }}"
                    supporting="{{ $waiting['reason'] }}"
                    overline="Backlog · {{ $waiting['project'] }}"
                    leadingIcon="hand.raised"
                    @press="openBacklogItem('{{ $waiting['project_id'] }}', '{{ $waiting['item_id'] }}')" />
            @endforeach

            @foreach ($questions as $question)
                @if ($question['rollback'])
                    <native:list-item native:key="rollback-{{ $question['id'] }}"
                        headline="Rollback waiting · {{ $question['title'] }}"
                        supporting="{{ $question['question'] }}"
                        overline="Release · {{ $this->assignment($question) }}"
                        leadingIcon="exclamationmark.triangle"
                        @press="openApproval('{{ $question['approval_id'] }}')" />
                    @continue
                @endif
                <native:list-item native:key="question-{{ $question['id'] }}"
                    headline="{{ $question['title'] }}"
                    supporting="{{ $question['question'] }}"
                    overline="Your lead asks · {{ $this->assignment($question) }}"
                    leadingIcon="bubble.left.and.bubble.right"
                    trailingIcon="chevron.right"
                    @navigate="'/conversations/' . $question['id']" />
            @endforeach
        </native:column>
    </native:scroll-view>

    {{-- The detail: beside the list on an iPad; in its place on a phone. --}}
    <native:scroll-view class="{{ $selectedApproval ? 'flex' : 'hidden md:flex' }} h-full flex-1 bg-theme-surface">
        @if ($selectedApproval)
            <native:column class="w-full gap-3 p-4">
                <native:button label="Back" variant="ghost" icon="chevron.left" class="md:hidden" @tap="closeDetail" />

                @if ($notice !== '')
                    <native:text ref="notice" class="text-sm text-theme-on-surface">{{ $notice }}</native:text>
                @endif
                @if (ReleaseApprovals::isRelease($selectedApproval))
                    <native:text class="text-sm text-theme-on-surface-variant">{{ ReleaseApprovals::headline($selectedApproval) }} · {{ $this->assignment($selectedApproval) }}</native:text>
                    @include('native.partials.release-approval', ['approval' => $selectedApproval])
                    @if ((string) ($selectedApproval['conversation_id'] ?? '') !== '')
                        <native:button label="Open the conversation" variant="ghost" icon="chevron.right" @tap="openConversation('{{ $selectedApproval['conversation_id'] }}')" />
                    @endif
                    @include('native.partials.release-owner-actions', ['approval' => $selectedApproval, 'screen' => $this])
                @elseif (BrowserActions::is($selectedApproval))
                    <native:text class="text-sm text-theme-on-surface-variant">{{ BrowserActions::headline($selectedApproval) }} · {{ $this->assignment($selectedApproval) }}</native:text>
                    <native:text class="text-sm font-bold text-theme-on-surface">Why the agent asks</native:text>
                    <native:text class="text-base text-theme-on-surface">{{ (string) ($selectedApproval['reason'] ?? '') }}</native:text>
                    <native:text class="text-xs text-theme-on-surface-variant">Job {{ (string) ($selectedApproval['job_id'] ?? '—') }} · run {{ (string) ($selectedApproval['run_id'] ?? '—') }}</native:text>
                    @include('native.partials.browser-action', ['approval' => $selectedApproval, 'screen' => $this])
                    @if (($selectedApproval['status'] ?? null) === 'needs_owner' && $this->mayDecideApproval($selectedApproval))
                        <native:text class="text-xs text-theme-on-surface-variant">Approving allows one submission of exactly this. Approving or denying asks for Face ID, Touch ID or your passcode first.</native:text>
                    @endif
                @else
                    <native:text class="text-sm text-theme-on-surface-variant">{{ ActionApprovals::status($selectedApproval) }} · {{ $this->assignment($selectedApproval) }}</native:text>
                    @if (ActionApprovals::isLand($selectedApproval))
                        @include('native.partials.land-approval', ['approval' => $selectedApproval])
                    @endif
                    <native:text ref="command" class="font-mono text-base text-theme-on-surface">{{ $command }}</native:text>
                    @if (ActionApprovals::malformed($selectedApproval))
                        <native:text class="text-sm text-theme-destructive">This request's command is malformed: an argument is not text. Deny it.</native:text>
                    @endif
                    @if (ActionApprovals::hidden($selectedApproval))
                        <native:text class="text-sm text-theme-destructive">The command contains invisible or text-reordering characters, shown above as \u{…}.</native:text>
                    @endif

                    @php($needs = \App\Client\Json::strings($selectedApproval['needs'] ?? null))
                    @if ($needs !== [])
                        <native:text class="text-sm font-bold text-theme-on-surface">It needs</native:text>
                        <native:text class="font-mono text-sm text-theme-on-surface">{{ implode(', ', $needs) }}</native:text>
                    @endif

                    @php($paths = \App\Client\Json::strings($selectedApproval['write_paths'] ?? null))
                    @if ($paths !== [])
                        <native:text class="text-sm font-bold text-theme-on-surface">Writes to</native:text>
                        @foreach ($paths as $path)
                            <native:text class="font-mono text-sm text-theme-on-surface">{{ $path }}</native:text>
                        @endforeach
                    @endif

                    <native:text class="text-sm font-bold text-theme-on-surface">Why the agent asks</native:text>
                    <native:text class="text-base text-theme-on-surface">{{ (string) ($selectedApproval['reason'] ?? '') }}</native:text>
                    <native:text class="text-sm text-theme-on-surface-variant">{{ (string) ($selectedApproval['requesting_model'] ?? '') }}</native:text>

                    <native:text class="text-sm font-bold text-theme-on-surface">Reviewer</native:text>
                    <native:text class="text-base text-theme-on-surface">{{ ActionApprovals::verdict($selectedApproval) }}</native:text>
                    @if (! empty($selectedApproval['review_reason']))
                        <native:text class="text-base text-theme-on-surface">{{ $selectedApproval['review_reason'] }}</native:text>
                    @endif

                    <native:text class="text-sm text-theme-on-surface-variant">{{ ActionApprovals::outcome($selectedApproval) }}</native:text>
                    @if (ActionApprovals::isLand($selectedApproval))
                        <native:text class="text-xs text-theme-on-surface-variant">Conversation {{ (string) ($selectedApproval['conversation_id'] ?? '—') }} · run {{ (string) ($selectedApproval['target_run_id'] ?? '—') }}</native:text>
                    @else
                        <native:text class="text-xs text-theme-on-surface-variant">Job {{ (string) ($selectedApproval['job_id'] ?? '—') }} · run {{ (string) ($selectedApproval['run_id'] ?? '—') }}</native:text>
                    @endif
                @endif

                @if (($selectedApproval['status'] ?? null) === 'needs_owner')
                    @if (! $this->mayDecideApproval($selectedApproval))
                        <native:text class="text-sm text-theme-on-surface-variant">You can read this request; deciding it is not yours on this Director.</native:text>
                    @elseif (ActionApprovals::decidable($selectedApproval))
                        <native:row class="w-full gap-3 mt-2">
                            <native:button label="Approve" variant="success" @tap="decide('{{ $selectedApproval['id'] }}', 'approve')" />
                            <native:button label="Deny" variant="destructive" @tap="decide('{{ $selectedApproval['id'] }}', 'deny')" />
                        </native:row>
                    @else
                        <native:text class="text-sm text-theme-on-surface-variant">{{ BrowserActions::is($selectedApproval) ? 'The decision window has ended, so it cannot be approved.' : 'The commit is not pinned yet, so it cannot be approved.' }}</native:text>
                        <native:button label="Deny" variant="destructive" @tap="decide('{{ $selectedApproval['id'] }}', 'deny')" />
                    @endif
                @endif
            </native:column>
        @else
            <native:column class="w-full h-full items-center justify-center p-8">
                <native:text class="text-base text-theme-on-surface-variant">Choose a request to see what it would run.</native:text>
            </native:column>
        @endif
    </native:scroll-view>
</native:row>
