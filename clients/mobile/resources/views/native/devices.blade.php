<native:top-bar title="Devices" back="true" />

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-3">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($loaded && ! $allowed)
                <native:text class="text-base text-theme-on-surface-variant">The Director does not let this credential manage devices. On the Director's machine, `cs device` can.</native:text>
            @elseif ($issued !== [])
                <native:text class="text-sm font-bold text-theme-on-surface">On {{ $issued['name'] }}, open ContextStack and enter</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">Director address</native:text>
                <native:text class="font-mono text-base text-theme-on-surface">{{ $address }}</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">Pairing code</native:text>
                <native:text ref="pairing-code" class="font-mono text-2xl text-theme-on-surface">{{ $issued['code'] }}</native:text>
                @if ($issuedQr !== '')
                    <native:text class="text-sm text-theme-on-surface-variant">Or scan this on the new device</native:text>
                    <native:image ref="pairing-qr" src="{{ $issuedQr }}" alt="QR code for the pairing link" class="w-64 h-64" />
                @endif
                @if (($issued['link'] ?? '') !== '')
                    <native:text class="text-sm text-theme-on-surface-variant">Link</native:text>
                    <native:text ref="pairing-link" class="font-mono text-sm text-theme-on-surface">{{ $issued['link'] }}</native:text>
                @endif
                <native:text class="text-sm text-theme-on-surface-variant">The code works once, for 10 minutes.</native:text>
                <native:button label="Done" class="w-full" @tap="done" />
            @else
                @if ($this->can('manage_own_devices'))
                <native:outlined-text-input label="New device name" placeholder="phone" ref="device-name"
                    native:model.debounce.300ms="name" autocapitalize="none" @submit="pair" />
                <native:button label="Get a pairing code" class="w-full" @tap="pair" />
                @endif

                <native:text class="mt-4 text-sm font-bold text-theme-on-surface">Paired devices</native:text>
                @foreach ($devices as $device)
                    @php($id = (string) ($device['id'] ?? ''))
                    @if ($id === $thisDevice)
                        <native:list-item native:key="device-{{ $id }}" headline="{{ $device['name'] ?? $id }}" supporting="{{ trim('This device · '.$this->owner($device), ' ·') }}" leadingIcon="ipad" />
                    @elseif (! empty($device['revoked_at']))
                        <native:list-item native:key="device-{{ $id }}" headline="{{ $device['name'] ?? $id }}" supporting="Revoked" leadingIcon="xmark.circle" />
                    @else
                        <native:list-item native:key="device-{{ $id }}" headline="{{ $device['name'] ?? $id }}" supporting="{{ trim('Paired · '.$this->owner($device), ' ·') }}"
                            leadingIcon="laptopcomputer.and.iphone"
                            :trailing-actions="[['method' => 'revoke(\''.$id.'\')', 'label' => 'Revoke', 'icon' => 'trash', 'role' => 'destructive']]" />
                    @endif
                @endforeach
                @if ($others > 0)
                    @if ($this->can('manage_member_devices'))
                        <native:list-item ref="others" headline="Other members' devices" supporting="{{ $others }} {{ $others === 1 ? 'device belongs' : 'devices belong' }} to other members: manage them in Members" leadingIcon="person.2" trailingIcon="chevron.right" @navigate='/members' />
                    @endif
                @endif
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
