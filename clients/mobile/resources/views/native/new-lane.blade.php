<native:top-bar title="New task lane" back="true" />

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-4">
            <native:outlined-text-input label="Title" native:model.debounce.300ms="title" />
            <native:outlined-text-input label="What should it do?" native:model.debounce.300ms="goal" multiline="true" min-lines="4" max-lines="12" />
            <native:select label="Repo" :options="$repos" native:model="repo" />
            @if (count($this->nodeChoices()) > 1)
                <native:select label="Machine" :options="$this->nodeChoices()" native:model="node" />
            @endif
            <native:outlined-text-input label="Lane name" native:model.debounce.300ms="lane" autocapitalize="none" />
            <native:select label="Model provider" :options="$this->providerChoices()" native:model="provider" />
            @if ($this->isAuto())
                <native:text class="text-sm text-theme-on-surface-variant">The Director plans first, then picks the model from the plan and records why.</native:text>
            @else
                <native:select label="Model" :options="$this->modelChoices()" native:model="model" />
                @if (count($this->harnessChoices()) > 1)
                    <native:select label="Runs in" :options="$this->harnessChoices()" native:model="harness" />
                @endif
                @if ($this->where() !== '')
                    <native:text class="text-xs text-theme-on-surface-variant">{{ $this->where() }}</native:text>
                @endif
                <native:select label="Plan first" :options="$this->planChoices()" native:model="planFirst" />
            @endif

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($this->can('start_work'))
                <native:button label="Start lane" variant="primary" class="w-full" @tap="start" />
            @else
                <native:text class="text-sm text-theme-on-surface-variant">Your role on this Director does not start work.</native:text>
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
