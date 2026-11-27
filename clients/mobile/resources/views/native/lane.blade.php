<native:top-bar title="{{ $this->title() }}" subtitle="{{ (string) ($job['state'] ?? '') }}" back="true">
    <native:top-bar-action id="refresh" label="Refresh" icon="refresh" @tap="refresh" />
</native:top-bar>

<native:scroll-view class="w-full h-full bg-theme-background">
    <native:column class="w-full px-4 md:px-24 lg:px-48 py-6">
        <native:column class="w-full gap-4">
            @if ($problem !== '')
                <native:text ref="problem" class="text-sm text-theme-destructive">{{ $problem }}</native:text>
            @endif
            @include('native.partials.signed-out')

            @if ($loaded && $job !== [])
                @php($route = $this->modelRoute())
                @if ($this->planning())
                    <native:text class="text-sm text-theme-on-surface-variant">This is the lane's planning job: it writes the plan the coding job follows, and is never landed itself.</native:text>
                @elseif ($this->canDecide())
                    <native:text class="text-base font-bold text-theme-on-surface">The run is reported. Approve or reject its result.</native:text>
                    <native:row class="w-full gap-3">
                        <native:button label="Approve" variant="success" @tap="approveRun" />
                        <native:button label="Reject" variant="destructive" @tap="rejectRun" />
                    </native:row>
                @elseif (($receipt['decision'] ?? null) !== null)
                    <native:text class="text-base text-theme-on-surface">Decision: {{ (string) $receipt['decision'] }}</native:text>
                @endif

                @if ($this->landed())
                    <native:text class="text-base font-bold text-theme-on-surface">Landed.</native:text>
                @elseif ($this->landCommand() !== '')
                    <native:text class="text-sm text-theme-on-surface-variant">Landing runs on the machine. To land it, run:</native:text>
                    <native:text class="font-mono text-base text-theme-on-surface">{{ $this->landCommand() }}</native:text>
                @endif

                <native:list-item headline="Model" supporting="{{ $route['model'] !== '' ? $route['model'] : '—' }}" />
                @if ($route['reason'] !== '')
                    <native:list-item headline="Model routing" supporting="{{ $route['reason'] }}" />
                @endif
                <native:list-item headline="Where" supporting="{{ $this->where() }}" />
                <native:list-item headline="Machine" supporting="{{ (string) ($job['node'] ?? '—') }}" />
                @if ($run !== [])
                    <native:list-item headline="Check verdict" supporting="{{ (string) ($run['verdict'] ?? '—') }}" />
                @endif
                <native:list-item headline="Started" supporting="{{ \App\Native\When::relative($job['created_at'] ?? null) }}" />
                @if (($job['finished_at'] ?? null) !== null)
                    <native:list-item headline="Finished" supporting="{{ \App\Native\When::relative($job['finished_at']) }}" />
                @endif

                @if (count($attempts) > 1)
                    <native:text class="text-sm font-bold text-theme-on-surface mt-2">Jobs in this lane</native:text>
                    @foreach ($attempts as $number => $attempt)
                        @if ($attempt['job_id'] === $jobId)
                            <native:list-item native:key="attempt-{{ $attempt['job_id'] }}"
                                headline="#{{ $number + 1 }} {{ $attempt['phase'] !== '' ? $attempt['phase'] : 'job' }} (this one)"
                                supporting="{{ $attempt['state'] }}" />
                        @else
                            <native:list-item native:key="attempt-{{ $attempt['job_id'] }}"
                                headline="#{{ $number + 1 }} {{ $attempt['phase'] !== '' ? $attempt['phase'] : 'job' }}"
                                supporting="{{ $attempt['state'] }}"
                                trailingIcon="chevron.right"
                                @navigate="'/lanes/' . $attempt['job_id']" />
                        @endif
                    @endforeach
                @endif

                @if ($this->cancelState() !== '')
                    <native:text class="text-sm text-theme-on-surface-variant">Cancel: {{ $this->cancelState() }}</native:text>
                @endif
                @if ($this->canCancel())
                    <native:button label="{{ $this->cancelState() === 'applied' ? 'Cancel again to finish this job' : 'Cancel this job' }}" variant="destructive" class="w-full mt-4" @tap="cancelJob" />
                @endif
            @endif
        </native:column>
    </native:column>
</native:scroll-view>
