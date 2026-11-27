<native:top-bar title="Connect" />

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-4">
            @if ($pairedWith !== '')
                <native:text class="text-sm text-theme-destructive">This device is already connected to {{ $pairedWith }}. Connecting again replaces that connection on this device; revoke the old one in Devices if you no longer need it.</native:text>
            @endif

            @if ($confirming)
                <native:text class="text-base font-bold text-theme-on-surface">Join this Director?</native:text>
                <native:text ref="confirm-url" class="font-mono text-base text-theme-on-surface">{{ $url }}</native:text>
                <native:text class="text-sm text-theme-on-surface-variant">
                    The invite joins this device to the Director at the address above, as the member it was issued for. Only join if you expected an invite to this Director.
                </native:text>

                <native:outlined-text-input label="This device's name" ref="device-name"
                    native:model.debounce.300ms="deviceName" autocapitalize="none" />

                @if ($problem !== '')
                    <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
                @endif

                <native:button label="Join this Director" variant="primary" class="w-full" @tap="confirmJoin" />
                <native:button label="Cancel" variant="secondary" class="w-full" @tap="cancelJoin" />
            @else
                <native:text class="text-base text-theme-on-surface-variant">
                    Connect this device to your Director once. Use a pairing code from Devices in any connected ContextStack app to add another device of yours, or an invite code someone sent you to join their Director. Scan its QR code, or enter the address and code here.
                </native:text>

                <native:select label="I have" :options="$this->haveChoices()" native:model="have" />

                <native:outlined-text-input label="Director address" placeholder="http://100.x.y.z:8780"
                    native:model.debounce.300ms="url" keyboard="url" autocapitalize="none" />

                <native:outlined-text-input label="{{ $this->inviting() ? 'Invite code or link' : 'Pairing code' }}" ref="code-input"
                    native:model.debounce.300ms="code" autocapitalize="characters" @submit="pair" />

                @if ($problem !== '')
                    <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
                @endif

                <native:button label="{{ $this->inviting() ? 'Join' : 'Pair' }}" @tap="pair" class="w-full" />

                <native:button label="Scan a code" variant="secondary" icon="qrcode" class="w-full" @tap="scan" />
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
