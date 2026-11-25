@use('App\NativeComponents\Machines')

<native:top-bar title="Machines" back="true">
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-2">
            <native:text class="text-sm text-theme-on-surface-variant">Machines pull their work. One not heard from within {{ $staleAfter }} seconds is stale.</native:text>

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($loaded && $machines === [])
                <native:text class="text-base text-theme-on-surface-variant">No machines have registered.</native:text>
            @endif

            @foreach ($machines as $machine)
                <native:list-item native:key="machine-{{ $machine['name'] }}"
                    headline="{{ $machine['name'] }}"
                    supporting="{{ Machines::summary($machine) }}"
                    leadingIcon="{{ $machine['state'] === 'revoked' ? 'xmark.circle' : ($machine['state'] === 'stale' ? 'exclamationmark.triangle' : 'desktopcomputer') }}" />
            @endforeach
        </native:column>
    </native:column>
</native:scroll-view>
