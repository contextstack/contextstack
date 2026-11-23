@use('App\Native\Backlog')

<native:top-bar title="{{ $projectName !== '' ? $projectName : 'Backlog' }}" subtitle="Backlog">
    <native:top-bar-action id="closed" label="{{ $showClosed ? 'Hide done' : 'Show done' }}" icon="{{ $showClosed ? 'eye.slash' : 'eye' }}" @tap="toggleClosed" />
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:row class="w-full h-full bg-theme-background">
    {{-- The list: always on an iPad; on a phone only while nothing is selected. --}}
    <native:scroll-view class="{{ $item ? 'hidden md:flex' : 'flex' }} h-full w-full md:w-96">
        <native:column class="w-full gap-2 p-4">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($this->waitingOnYou() !== [])
                <native:text class="text-sm font-bold text-theme-on-surface">Waiting on you</native:text>
                @foreach ($this->waitingOnYou() as $waiting)
                    <native:list-item native:key="waiting-{{ $waiting['item_id'] }}"
                        headline="{{ $waiting['title'] }}"
                        supporting="{{ (string) ($waiting['blocked_reason'] ?? '') }}"
                        leading-icon="hand.raised"
                        @press="select('{{ $waiting['item_id'] }}')" />
                @endforeach
                <native:divider class="w-full my-2" />
            @endif

            @if ($loaded && $this->shown() === [])
                <native:text class="text-base text-theme-on-surface-variant">Nothing in this backlog.</native:text>
            @endif

            @foreach ($this->shown() as $row)
                <native:list-item native:key="item-{{ $row['item_id'] }}"
                    headline="{{ $row['title'] }}"
                    supporting="{{ Backlog::blockers($row) !== '' ? 'Blocked on '.Backlog::blockers($row) : '' }}"
                    trailing-text="{{ Backlog::status($row) }}"
                    @press="select('{{ $row['item_id'] }}')" />
            @endforeach
        </native:column>
    </native:scroll-view>

    {{-- The item: beside the list on an iPad; in its place on a phone. --}}
    <native:scroll-view class="{{ $item ? 'flex' : 'hidden md:flex' }} h-full flex-1 bg-theme-surface">
        @if ($item)
            <native:column class="w-full gap-3 p-4">
                <native:button label="Back" variant="ghost" icon="chevron.left" class="md:hidden" @tap="closeDetail" />

                <native:text class="text-sm text-theme-on-surface-variant">{{ Backlog::status($item) }}</native:text>
                <native:text ref="item-title" class="text-xl font-bold text-theme-on-surface">{{ $item['title'] }}</native:text>

                @if (($item['body'] ?? '') !== '')
                    <native:text class="text-base text-theme-on-surface">{{ $item['body'] }}</native:text>
                @endif

                @if (Backlog::blockers($item) !== '')
                    <native:text class="text-sm font-bold text-theme-on-surface">Blocked on</native:text>
                    <native:text class="text-base text-theme-on-surface">{{ Backlog::blockers($item) }}</native:text>
                    @if (($item['blocked_reason'] ?? '') !== '')
                        <native:text class="text-base text-theme-on-surface">{{ $item['blocked_reason'] }}</native:text>
                    @endif
                @endif

                @if (Backlog::waitsOnOwner($item))
                    <native:button label="I've answered: clear it" variant="primary" class="w-full mt-2" @tap="clearOwnerBlocker('{{ $item['item_id'] }}')" />
                @endif

                <native:row class="w-full gap-3 mt-2">
                    @if (in_array($item['status'] ?? 'next', ['next', 'blocked'], true))
                        <native:button label="Start" variant="secondary" class="flex-1" @tap="setStatus('{{ $item['item_id'] }}', 'now')" />
                    @endif
                    @if (in_array($item['status'] ?? 'next', Backlog::Open, true))
                        <native:button label="Done" variant="success" class="flex-1" @tap="setStatus('{{ $item['item_id'] }}', 'done')" />
                        <native:button label="Drop" variant="destructive" class="flex-1" @tap="setStatus('{{ $item['item_id'] }}', 'dropped')" />
                    @else
                        <native:button label="Reopen" variant="secondary" class="flex-1" @tap="setStatus('{{ $item['item_id'] }}', 'next')" />
                    @endif
                </native:row>
            </native:column>
        @else
            <native:column class="w-full h-full items-center justify-center p-8">
                <native:text class="text-base text-theme-on-surface-variant">Choose an item to see it here.</native:text>
            </native:column>
        @endif
    </native:scroll-view>
</native:row>
