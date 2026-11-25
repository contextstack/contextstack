@use('App\NativeComponents\Usage')

<native:top-bar title="Usage" back="true">
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-2">
            <native:select label="Period" :options="$this->periodChoices()" native:model="period" />
            <native:text class="text-sm text-theme-on-surface-variant">Each source is shown for what it is: pay as you go with the dollars charged, a subscription with its allowance, local with usage only. Estimates are never shown.</native:text>
            @if ($range !== '')
                <native:text class="text-xs text-theme-on-surface-variant">{{ $range }}</native:text>
            @endif

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($loaded && $sources === [])
                <native:text class="text-base text-theme-on-surface-variant">No runs in this period.</native:text>
            @endif

            @foreach ($sources as $index => $source)
                <native:list-item native:key="source-{{ $index }}"
                    headline="{{ $source['model'] }}"
                    supporting="{{ Usage::summary($source) }}" />
            @endforeach

            @if ($total !== [])
                <native:text class="text-base font-bold text-theme-on-surface mt-4">Total</native:text>
                @foreach ($total as $line)
                    <native:text class="text-sm text-theme-on-surface">{{ $line }}</native:text>
                @endforeach
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
