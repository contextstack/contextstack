@use('App\NativeComponents\Conversation')

<native:top-bar title="{{ $this->title() }}" subtitle="{{ $this->subtitle() }}" back="true">
    @if (! $this->drives('control_work'))
    @elseif ($this->held())
        <native:top-bar-action id="resume" label="Go on" icon="play.circle" @tap="resumeConversation" />
    @else
        <native:top-bar-action id="stop" label="Stop" icon="stop.circle" @tap="stopConversation" />
    @endif
    @if ($this->mayArchive() && ! $this->archived())
        <native:top-bar-action id="archive" label="Archive" icon="archivebox" @tap="askArchive" />
    @endif
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-4">
        <native:column class="w-full gap-4">
            @if ($this->sponsorship() !== '')
                <native:text ref="sponsorship" class="text-sm text-theme-on-surface-variant">{{ $this->sponsorship() }}</native:text>
            @endif
            @if ($this->mayAdopt())
                <native:text class="text-xs text-theme-on-surface-variant">Adopting makes you this lead's sponsor and revokes its current lead credentials; its next turn runs under your role.</native:text>
                <native:button label="Adopt this conversation" variant="secondary" class="w-full" @tap="adopt" />
            @endif

            @if ($this->archived())
                <native:text ref="archived" class="text-sm text-theme-on-surface-variant">{{ $notice !== '' ? $notice : 'This conversation is archived. Unarchive it to send messages.' }}</native:text>
                @if ($this->mayArchive())
                    <native:button label="Unarchive" variant="secondary" class="w-full" @tap="unarchiveConversation" />
                @endif
            @elseif ($confirmingArchive)
                <native:text ref="archive-confirm" class="text-sm text-theme-on-surface">Archive this conversation? It leaves the Conversations list and takes no messages until it is unarchived. Its thread stays readable.</native:text>
                <native:row class="w-full gap-3">
                    <native:button label="Archive it" variant="destructive" @tap="archiveConversation" />
                    <native:button label="Cancel" variant="ghost" @tap="cancelArchive" />
                </native:row>
            @endif

            @php($attention = $this->attention())
            @if ($attention !== [])
                @include('native.partials.attention', ['attention' => $attention, 'screen' => $this])
            @endif
            @if ($browserOutcome !== [])
                <native:column ref="browser-outcome-card" class="w-full gap-2 p-3 rounded-lg bg-theme-surface">
                    <native:text class="text-sm font-bold text-theme-on-surface">{{ \App\Native\BrowserActions::headline($browserOutcome) }}</native:text>
                    <native:text class="text-sm text-theme-on-surface">{{ \App\Native\BrowserActions::status($browserOutcome) }}</native:text>
                    <native:text class="text-sm text-theme-on-surface-variant">{{ \App\Native\BrowserActions::outcome($browserOutcome) }}</native:text>
                </native:column>
            @endif

            @if ($this->drives('start_work') && ! $this->archived())
                <native:outlined-text-input label="Message" ref="message-input"
                    native:model.debounce.300ms="message" multiline="true" min-lines="2" max-lines="8" />
                <native:row class="w-full gap-3 items-center">
                    <native:button label="Send" variant="primary" icon="paperplane.fill" @tap="post" />
                    @if ($notice !== '')
                        <native:text class="text-sm text-theme-on-surface-variant">{{ $notice }}</native:text>
                    @endif
                </native:row>
            @endif

            @if ($this->held())
                <native:row class="w-full gap-3 items-center">
                    <native:text class="text-sm text-theme-on-surface-variant">Stopped: new messages wait until you tap Go on.</native:text>
                    @if ($this->drives('control_work'))
                        <native:button label="Go on" variant="secondary" icon="play.fill" @tap="resumeConversation" />
                    @endif
                </native:row>
            @endif

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($loaded && $turns === [])
                <native:text class="text-base text-theme-on-surface-variant">Nothing said yet.</native:text>
            @endif

            @foreach ($turns as $turn)
                <native:column native:key="turn-{{ $turn['turn_id'] ?? $loop->index }}" class="w-full gap-2 py-2">
                    <native:divider class="w-full" />
                    @if (Conversation::peer($turn) !== '')
                        <native:text class="text-xs font-bold text-theme-destructive">{{ Conversation::sender($turn) }}</native:text>
                        <native:text class="text-base text-theme-on-surface">{{ (string) ($turn['message'] ?? '') }}</native:text>
                    @else
                        <native:text class="text-xs text-theme-on-surface-variant">{{ Conversation::sender($turn) }}</native:text>
                        <native:text class="text-base font-bold text-theme-on-surface">{{ (string) ($turn['message'] ?? '') }}</native:text>
                    @endif
                    <native:text class="text-base {{ Conversation::failed($turn) ? 'text-theme-destructive' : 'text-theme-on-surface' }}">{{ Conversation::answer($turn) }}</native:text>
                </native:column>
            @endforeach
        </native:column>
    </native:column>
</native:scroll-view>
