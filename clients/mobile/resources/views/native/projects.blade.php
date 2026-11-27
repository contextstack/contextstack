<native:top-bar title="Projects" />

@include('native.partials.tabs')

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full p-4 md:px-24 lg:px-48">
        <native:column class="w-full gap-2">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($loaded && $projects === [])
                <native:text class="text-base text-theme-on-surface-variant">No projects yet.</native:text>
            @endif

            @foreach ($projects as $project)
                <native:list-item native:key="project-{{ $project['id'] }}"
                    headline="{{ $project['name'] }}"
                    supporting="Backlog"
                    leadingIcon="folder"
                    trailingIcon="chevron.right"
                    @navigate="'/projects/' . $project['id']" />
            @endforeach
        </native:column>
    </native:column>
</native:scroll-view>
