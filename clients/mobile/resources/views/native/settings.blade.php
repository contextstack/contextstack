<native:top-bar title="Settings" />

@include('native.partials.tabs')

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-2">
            <native:list-item headline="Director" supporting="{{ $url }}" leading-icon="server" />
            <native:list-item headline="This device" supporting="{{ $name !== '' ? $name : 'Paired device' }}" leading-icon="phone" />
            <native:list-item headline="Credential" supporting="{{ $credentialId }}" leading-icon="key" />
            <native:list-item headline="Devices" supporting="Pair or revoke devices" leading-icon="laptopcomputer.and.iphone" trailing-icon="chevron.right" @navigate='/devices' />

            <native:text class="mt-4 text-sm font-bold text-theme-on-surface">App lock</native:text>
            <native:text class="text-sm text-theme-on-surface-variant">
                Ask for Face ID, Touch ID or the passcode, whichever this device uses, when ContextStack opens and after it has been away.
            </native:text>
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @if ($lockEnabled)
                @foreach ($lockChoices as $seconds => $label)
                    <native:list-item native:key="lock-after-{{ $seconds }}" headline="{{ $label }}"
                        leading-icon="{{ $seconds === $lockAfter ? 'checkmark.circle.fill' : 'circle' }}"
                        @press="lockAfterSeconds({{ $seconds }})" />
                @endforeach
                <native:button label="Turn off app lock" class="w-full" @tap="turnOffLock" />
            @else
                <native:button label="Turn on app lock" variant="primary" class="w-full" @tap="turnOnLock" />
            @endif

            <native:text class="mt-4 text-sm text-theme-on-surface-variant">
                Forgetting removes this device's token from it. If the device is lost, also revoke the credential on the Director's machine.
            </native:text>
            <native:button label="Forget this Director" variant="destructive" class="w-full" @tap="forget" />
        </native:column>
    </native:column>
</native:scroll-view>
