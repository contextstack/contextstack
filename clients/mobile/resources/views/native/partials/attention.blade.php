@use('App\Native\NeedsYou')
{{-- A conversation's needs-you item: the question, who it waits on, its options (default highlighted, sent only on a press) and links to the records it names. --}}
@php($options = $screen->offeredOptions())
@php($default = NeedsYou::default($attention))
<native:column class="w-full gap-2 p-3 rounded-lg bg-theme-surface">
    <native:text ref="attention-assignment" class="text-xs font-bold text-theme-on-surface-variant">{{ $screen->assignment($attention) }}</native:text>
    @if (in_array($attention['kind'] ?? null, ['install', 'rollback'], true))
        <native:text ref="attention-release" class="text-sm font-bold text-theme-on-surface">{{ ($attention['options'][0]['id'] ?? null) === 'acknowledge' ? 'Release rollback waiting on jobs' : (($attention['kind'] ?? null) === 'install' ? 'Release install approval' : 'Release rollback approval') }}</native:text>
    @endif
    @if ((string) ($attention['question'] ?? '') !== '')
        <native:text ref="attention-question" class="text-base text-theme-on-surface">{{ $attention['question'] }}</native:text>
    @endif
    @if (($attention['kind'] ?? null) === 'browser_action')
        <native:text ref="attention-browser" class="text-sm font-bold text-theme-on-surface">Browser action approval</native:text>
        @if (isset($attention['origin']))
            @include('native.partials.browser-action', ['approval' => array_merge(['status' => 'needs_owner'], $attention), 'screen' => $screen])
        @else
            <native:text class="text-sm text-theme-on-surface-variant">Only an owner sees what this browser action would submit.</native:text>
        @endif
    @endif
    @if ($options !== [] && $screen->mayAnswer())
        <native:row class="w-full gap-2">
            @foreach ($options as $option)
                <native:button native:key="option-{{ $option['id'] }}" label="{{ $option['label'] }}"
                    variant="{{ $option['id'] === $default ? 'primary' : 'secondary' }}"
                    @tap="choose('{{ $option['id'] }}')" />
            @endforeach
        </native:row>
        @if (in_array($attention['kind'] ?? null, ['install', 'rollback'], true) && ($attention['options'][0]['id'] ?? null) !== 'acknowledge')
            <native:text ref="attention-release-note" class="text-xs text-theme-on-surface-variant">Open the approval to read the pinned commit, schema and notes first.{{ ($attention['kind'] ?? null) === 'install' ? ' Go also approves restoring the previous release if the smoke trial fails.' : '' }} Go and No-go ask for Face ID, Touch ID or your passcode.</native:text>
        @endif
        @if (($attention['kind'] ?? null) === 'browser_action')
            <native:text ref="attention-browser-note" class="text-xs text-theme-on-surface-variant">Go approves one submission of exactly this; No-go denies it. Both ask for Face ID, Touch ID or your passcode.</native:text>
        @endif
    @elseif ($options !== [])
        <native:text class="text-sm text-theme-on-surface-variant">Choices: {{ implode(', ', array_column($options, 'label')) }}. Answering it is not yours on this Director.</native:text>
    @endif
    @foreach (NeedsYou::links($attention) as $kind => $id)
        <native:button native:key="link-{{ $kind }}" label="{{ NeedsYou::linkWords($kind) }}" variant="ghost" icon="chevron.right"
            @tap="openLink('{{ $kind }}', '{{ $id }}', '{{ (string) ($attention['project_id'] ?? '') }}')" />
    @endforeach
</native:column>
