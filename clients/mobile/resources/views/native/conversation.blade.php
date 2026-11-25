@use('App\NativeComponents\Conversation')

<native:top-bar title="{{ $this->title() }}" subtitle="{{ $this->subtitle() }}" back="true">
    @if ($this->held())
        <native:top-bar-action id="resume" label="Go on" icon="play.circle" @tap="resumeConversation" />
    @else
        <native:top-bar-action id="stop" label="Stop" icon="stop.circle" @tap="stopConversation" />
    @endif
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-4">
        <native:column class="w-full gap-4">
            <native:outlined-text-input label="Message" ref="message-input"
                native:model.debounce.300ms="message" multiline="true" min-lines="2" max-lines="8" />
            <native:row class="w-full gap-3 items-center">
                <native:button label="Send" variant="primary" icon="paperplane.fill" @tap="post" />
                @if ($notice !== '')
                    <native:text class="text-sm text-theme-on-surface-variant">{{ $notice }}</native:text>
                @endif
            </native:row>

            @if ($this->held())
                <native:row class="w-full gap-3 items-center">
                    <native:text class="text-sm text-theme-on-surface-variant">Stopped: new messages wait until you tap Go on.</native:text>
                    <native:button label="Go on" variant="secondary" icon="play.fill" @tap="resumeConversation" />
                </native:row>
            @endif

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($loaded && $turns === [])
                <native:text class="text-base text-theme-on-surface-variant">Nothing said yet.</native:text>
            @endif

            @foreach ($turns as $turn)
                <native:column native:key="turn-{{ $turn['turn_id'] ?? $loop->index }}" class="w-full gap-2 py-2">
                    <native:divider class="w-full" />
                    <native:text class="text-xs text-theme-on-surface-variant">{{ Conversation::sender($turn) }}</native:text>
                    <native:text class="text-base font-bold text-theme-on-surface">{{ (string) ($turn['message'] ?? '') }}</native:text>
                    <native:text class="text-base {{ Conversation::failed($turn) ? 'text-theme-destructive' : 'text-theme-on-surface' }}">{{ Conversation::answer($turn) }}</native:text>
                </native:column>
            @endforeach
        </native:column>
    </native:column>
</native:scroll-view>
