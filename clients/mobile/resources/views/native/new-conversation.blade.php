<native:top-bar title="New conversation" back="true" />

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-4">
            <native:select label="Kind" :options="$this->kindChoices()" native:model="kind" />
            @if ($this->isLead())
                <native:text class="text-sm text-theme-on-surface-variant">A lead runs on the Director's home machine and can start and watch task lanes for you. Only models that can lead are offered.</native:text>
            @endif
            <native:select label="Provider" :options="$this->providerChoices()" native:model="provider" />
            <native:select label="Model" :options="$this->modelChoices()" native:model="model" />
                @if (count($this->harnessChoices()) > 1)
                    <native:select label="Runs in" :options="$this->harnessChoices()" native:model="harness" />
                @endif
            @if ($this->where() !== '')
                <native:text class="text-xs text-theme-on-surface-variant">{{ $this->where() }}</native:text>
            @endif
            <native:outlined-text-input label="Title (optional)" native:model.debounce.300ms="title" />
            <native:outlined-text-input label="First message" native:model.debounce.300ms="message" multiline="true" min-lines="3" max-lines="10" />

            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($this->can('start_work'))
                <native:button label="Start" variant="primary" class="w-full" @tap="start" />
            @else
                <native:text class="text-sm text-theme-on-surface-variant">Your role on this Director does not start work.</native:text>
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
