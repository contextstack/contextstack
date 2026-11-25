<native:top-bar title="Source budgets" back="true">
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-2">
            <native:text class="text-sm text-theme-on-surface-variant">Each pay-as-you-go source needs a budget; a source without one is off. Subscriptions and local models have no dollars to budget. Changing a budget asks for Face ID, Touch ID or your passcode.</native:text>

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($loaded)
                <native:list-item native:key="source-shared"
                    headline="Shared budget"
                    supporting="{{ $shared !== '' ? $shared.' · covers every source without its own' : 'None' }}"
                    leadingIcon="tray.full"
                    trailingIcon="chevron.right"
                    @press="edit('*')" />

                @foreach ($sources as $source)
                    <native:list-item native:key="source-{{ $source['source'] }}"
                        headline="{{ $source['source'] }}"
                        supporting="{{ $source['state'] === 'active' ? 'active' : ($source['state'] === 'exhausted' ? 'budget used up' : 'off') }} · {{ $source['budget'] }} · {{ $source['spent'] }}"
                        leadingIcon="{{ $source['state'] === 'active' ? 'checkmark.circle' : ($source['state'] === 'exhausted' ? 'exclamationmark.circle' : 'circle') }}"
                        trailingIcon="chevron.right"
                        @press="edit('{{ $source['source'] }}')" />
                @endforeach
            @endif

            @if ($editing !== '')
                <native:divider class="w-full my-2" />
                <native:text class="text-base font-bold text-theme-on-surface">{{ $editing === '*' ? 'Shared budget' : $editing }}</native:text>
                <native:outlined-text-input label="Dollars" native:model.debounce.300ms="usd" keyboard="decimal" />
                <native:select label="Period" :options="$this->periodChoices()" native:model="period" />
                <native:row class="w-full gap-3">
                    <native:button label="Set budget" variant="primary" @tap="setBudget" />
                    <native:button label="Turn off" variant="destructive" @tap="turnOff('{{ $editing }}')" />
                    <native:button label="Close" variant="secondary" @tap="closeEdit" />
                </native:row>
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
