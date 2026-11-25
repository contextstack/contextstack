@use('App\NativeComponents\Conversations')

<native:top-bar title="Conversations">
    <native:top-bar-action id="new" label="New conversation" icon="plus" @tap="newConversation" />
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

@include('native.partials.tabs')

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full p-4 md:px-24 lg:px-48">
        <native:column class="w-full gap-2">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($loaded && $conversations === [])
                <native:text class="text-base text-theme-on-surface-variant">No conversations yet. Tap + to start one.</native:text>
            @endif

            @foreach ($conversations as $conversation)
                <native:list-item native:key="conversation-{{ $conversation['id'] }}"
                    headline="{{ Conversations::title($conversation) }}"
                    supporting="{{ Conversations::summary($conversation) }}"
                    leadingIcon="{{ $conversation['lead'] ? 'person.crop.circle.badge.checkmark' : 'bubble.left.and.bubble.right' }}"
                    trailingIcon="chevron.right"
                    @navigate="'/conversations/' . $conversation['id']" />
            @endforeach
        </native:column>
    </native:column>
</native:scroll-view>
