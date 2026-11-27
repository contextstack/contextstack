@use('App\Native\BrowserActions')
{{-- A browser action approval (or the needs-you item that carries one): where and what it would submit, every captured field in order, the screenshot taken at proposal time, the window, and where it stands. All page text is plain text. The masked-fields warning comes last, next to the buttons that follow. --}}
@php($fields = BrowserActions::fields($approval))
@php($image = $screen->screenshotOf($approval))
@php($sha = BrowserActions::sha($approval))
<native:text ref="browser-status" class="text-sm font-bold text-theme-on-surface">{{ BrowserActions::status($approval) }}</native:text>

<native:text class="text-sm font-bold text-theme-on-surface">Action</native:text>
<native:text ref="browser-action" class="text-base text-theme-on-surface">{{ BrowserActions::actionWords($approval) }}</native:text>
<native:text class="text-sm font-bold text-theme-on-surface">Origin</native:text>
<native:text ref="browser-origin" class="font-mono text-sm text-theme-on-surface">{{ BrowserActions::origin($approval) }}</native:text>
<native:text class="text-sm font-bold text-theme-on-surface">Target</native:text>
<native:text ref="browser-target" class="font-mono text-sm text-theme-on-surface">{{ BrowserActions::target($approval) }}</native:text>

<native:text class="text-sm font-bold text-theme-on-surface">{{ $fields === [] ? 'No fields were captured' : 'Fields, in page order ('.count($fields).')' }}</native:text>
@foreach ($fields as $index => $field)
    <native:column native:key="field-{{ $index }}" class="w-full gap-0">
        <native:text class="font-mono text-sm font-bold text-theme-on-surface">{{ $field['name'] }}{{ $field['labels'] !== '' ? ' · '.$field['labels'] : '' }}</native:text>
        <native:text class="font-mono text-sm {{ $field['masked'] ? 'text-theme-on-surface-variant' : 'text-theme-on-surface' }}">{{ $field['shown'] }}</native:text>
    </native:column>
@endforeach

<native:text class="text-sm font-bold text-theme-on-surface">Screenshot at proposal time</native:text>
@if ($image !== '')
    <native:pressable ref="browser-screenshot" class="w-full gap-1" @navigate="'/browser-screenshots/' . $sha">
        <native:image src="{{ $image }}" alt="Screenshot of the page the browser action would act on" fit="contain" class="w-full h-64" />
        <native:text class="text-xs text-theme-primary">Tap to view full size</native:text>
    </native:pressable>
@elseif ($screen->screenshotProblem !== '')
    <native:text ref="browser-screenshot-problem" class="text-sm text-theme-destructive">{{ $screen->screenshotProblem }}</native:text>
@else
    <native:text class="text-sm text-theme-on-surface-variant">Loading the screenshot.</native:text>
@endif

<native:text ref="browser-window" class="text-sm text-theme-on-surface">{{ BrowserActions::window($approval) }}</native:text>
@if (BrowserActions::outcome($approval) !== '')
    <native:text ref="browser-outcome" class="text-sm text-theme-on-surface-variant">{{ BrowserActions::outcome($approval) }}</native:text>
@endif
@if (($approval['status'] ?? null) === 'approved' && (string) ($approval['job_id'] ?? '') !== '')
    {{-- The follow-up has no lane of its own: it is a job of the proposing job's task, listed in that task's lane. --}}
    <native:button ref="browser-open-task" label="Open the task" variant="ghost" icon="chevron.right" @tap="openLink('job', '{{ $approval['job_id'] }}')" />
@endif

@if (BrowserActions::warning($approval) !== '')
    <native:text ref="browser-field-warning" class="text-base font-bold text-theme-destructive">{{ BrowserActions::warning($approval) }}</native:text>
@endif
