<native:top-bar title="ContextStack" />

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-12">
        <native:column class="w-full gap-4 items-center">
            <native:icon name="lock.fill" size="48" />
            <native:text class="text-xl font-bold text-theme-on-surface">Locked</native:text>
            <native:text class="text-sm text-center text-theme-on-surface-variant">Unlock with Face ID, Touch ID or your passcode, whichever this device uses.</native:text>

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            <native:button label="Unlock" variant="primary" class="w-full" @tap="unlock" />

            <native:text class="mt-8 text-sm text-center text-theme-on-surface-variant">Can't unlock this device any more? Forgetting the Director takes its token off this device. Pair it again afterwards.</native:text>
            <native:button label="Forget this Director" variant="destructive" class="w-full" @tap="forget" />
        </native:column>
    </native:column>
</native:scroll-view>
