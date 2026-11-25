@use('App\NativeComponents\Lanes')

<native:top-bar title="Task lanes">
    <native:top-bar-action id="new" label="New lane" icon="plus" @tap="newLane" />
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

@include('native.partials.tabs')

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full p-4 md:px-24 lg:px-48">
        <native:column class="w-full gap-2">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif

            @if ($loaded && $lanes === [])
                <native:text class="text-base text-theme-on-surface-variant">No task lanes yet. Tap + to start one.</native:text>
            @endif

            @foreach ($lanes as $lane)
                <native:list-item native:key="lane-{{ $lane['job_id'] }}"
                    headline="{{ $lane['title'] }}"
                    supporting="{{ Lanes::summary($lane) }}"
                    leadingIcon="hammer"
                    trailingIcon="chevron.right"
                    @navigate="'/lanes/' . $lane['job_id']" />
            @endforeach
        </native:column>
    </native:column>
</native:scroll-view>
