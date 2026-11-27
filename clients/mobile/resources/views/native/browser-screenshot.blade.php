<native:top-bar title="Screenshot" back="true">
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:column class="w-full h-full gap-2 p-4 bg-theme-background">
    @include('native.partials.signed-out')
    @if ($screenshotProblem !== '')
        <native:text ref="screenshot-problem" class="text-sm text-theme-destructive">{{ $screenshotProblem }}</native:text>
    @endif
    @if ($image !== '')
        <native:text class="text-xs text-theme-on-surface-variant">Captured by the agent's browser when it proposed the action. It is fitted to the screen.</native:text>
        <native:image ref="screenshot-full" src="{{ $image }}" alt="Screenshot of the page the browser action would act on" fit="contain" class="w-full flex-1" />
    @endif
</native:column>
