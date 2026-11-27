{{-- A 401: the screen has cleared what it read from the Director; offer to pair again or forget it. --}}
@if ($signedOut)
    <native:row class="w-full gap-3">
        <native:button label="Pair again" variant="primary" class="flex-1" @tap="reconnect" />
        <native:button label="Forget this Director" variant="destructive" class="flex-1" @tap="forgetDirector" />
    </native:row>
@endif
