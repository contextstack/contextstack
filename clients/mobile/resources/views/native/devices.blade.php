<native:top-bar title="Devices" />

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-3">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($loaded && ! $allowed)
                <native:text class="text-base text-theme-on-surface-variant">The Director does not let this credential manage devices. Use the desktop app, or `cs device` on the Director's machine.</native:text>
            @elseif ($issued !== [])
                <native:text class="text-sm font-bold text-theme-on-surface">On {{ $issued['name'] }}, open ContextStack and enter</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">Director address</native:text>
                <native:text class="font-mono text-base text-theme-on-surface">{{ $address }}</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">Pairing code</native:text>
                <native:text ref="pairing-code" class="font-mono text-2xl text-theme-on-surface">{{ $issued['code'] }}</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">The code works once, for 10 minutes.</native:text>
                <native:button label="Done" class="w-full" @tap="done" />
            @else
                <native:outlined-text-input label="New device name" placeholder="phone" ref="device-name"
                    native:model.debounce.300ms="name" autocapitalize="none" @submit="pair" />
                <native:button label="Get a pairing code" class="w-full" @tap="pair" />

                <native:text class="mt-4 text-sm font-bold text-theme-on-surface">Paired devices</native:text>
                @foreach ($devices as $device)
                    @php($id = (string) ($device['id'] ?? ''))
                    @if ($id === $thisDevice)
                        <native:list-item native:key="device-{{ $id }}" headline="{{ $device['name'] ?? $id }}" supporting="This device" leading-icon="ipad" />
                    @elseif (! empty($device['revoked_at']))
                        <native:list-item native:key="device-{{ $id }}" headline="{{ $device['name'] ?? $id }}" supporting="Revoked" leading-icon="xmark.circle" />
                    @else
                        <native:list-item native:key="device-{{ $id }}" headline="{{ $device['name'] ?? $id }}" supporting="Paired"
                            leading-icon="laptopcomputer.and.iphone"
                            :trailing-actions="[['method' => 'revoke(\''.$id.'\')', 'label' => 'Revoke', 'icon' => 'trash', 'role' => 'destructive']]" />
                    @endif
                @endforeach
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
