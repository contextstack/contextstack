<native:top-bar title="Connect" />

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-4">
            <native:text class="text-base text-theme-on-surface-variant">
                Pair this device with your Director once. In the ContextStack desktop app on the Director's machine, open Devices and choose Get a pairing code, then scan its QR code, or enter the address and code it shows here, within 10 minutes.
            </native:text>

            <native:outlined-text-input label="Director address" placeholder="http://100.x.y.z:8780"
                native:model.debounce.300ms="url" keyboard="url" autocapitalize="none" />

            <native:outlined-text-input label="Pairing code" ref="code-input"
                native:model.debounce.300ms="code" autocapitalize="characters" @submit="pair" />

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            <native:button label="Pair" @tap="pair" class="w-full" />

            <native:button label="Scan pairing code" variant="secondary" icon="qrcode" class="w-full" @tap="scan" />
        </native:column>
    </native:column>
</native:scroll-view>
