@use('App\Native\ActionApprovals')

<native:top-bar title="Waiting on you" subtitle="{{ $loaded ? ($count === 0 ? 'Nothing waits on you' : $count.' waiting') : 'Loading' }}">
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

@include('native.partials.tabs', ['waitingCount' => $count])

<native:row class="w-full h-full bg-theme-background">
    {{-- The list: always on an iPad; on a phone only while nothing is selected. --}}
    <native:scroll-view class="{{ $selectedApproval ? 'hidden md:flex' : 'flex' }} h-full w-full md:w-96">
        <native:column class="w-full gap-2 p-4">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($backlogProblem !== '')
                <native:text class="text-sm text-theme-on-surface-variant">{{ $backlogProblem }}</native:text>
            @endif

            @if ($loaded && $count === 0)
                <native:text class="text-base text-theme-on-surface-variant">Nothing waits on you.</native:text>
            @endif

            @foreach ($approvals as $id => $approval)
                <native:list-item native:key="approval-{{ $id }}"
                    headline="{{ ActionApprovals::command($approval) }}"
                    supporting="{{ (string) ($approval['reason'] ?? '') }}"
                    overline="Action approval"
                    leadingIcon="shield"
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
                <native:list-item native:key="question-{{ $question['id'] }}"
                    headline="{{ $question['title'] }}"
                    supporting="{{ $question['question'] }}"
                    overline="Your lead asks"
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

                <native:text class="text-sm text-theme-on-surface-variant">{{ ActionApprovals::status($selectedApproval) }}</native:text>
                <native:text ref="command" class="font-mono text-base text-theme-on-surface">{{ $command }}</native:text>
                @if (ActionApprovals::malformed($selectedApproval))
                    <native:text class="text-sm text-theme-destructive">This request's command is malformed: an argument is not text. Deny it, or check it on the desktop.</native:text>
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
                <native:text class="text-xs text-theme-on-surface-variant">Job {{ (string) ($selectedApproval['job_id'] ?? '—') }} · run {{ (string) ($selectedApproval['run_id'] ?? '—') }}</native:text>

                @if (($selectedApproval['status'] ?? null) === 'needs_owner')
                    <native:row class="w-full gap-3 mt-2">
                        <native:button label="Approve" variant="success" @tap="decide('{{ $selectedApproval['id'] }}', 'approve')" />
                        <native:button label="Deny" variant="destructive" @tap="decide('{{ $selectedApproval['id'] }}', 'deny')" />
                    </native:row>
                @endif
            </native:column>
        @else
            <native:column class="w-full h-full items-center justify-center p-8">
                <native:text class="text-base text-theme-on-surface-variant">Choose a request to see what it would run.</native:text>
            </native:column>
        @endif
    </native:scroll-view>
</native:row>
