@use('App\Native\ActionApprovals')
{{-- A hosted land approval: what would land, pinned to the full commit, and both path flags spelled out. --}}
<native:text class="text-sm font-bold text-theme-on-surface">Repository</native:text>
<native:text ref="land-repo" class="font-mono text-base text-theme-on-surface">{{ (string) ($approval['repo'] ?? '—') }}</native:text>
<native:text class="text-sm font-bold text-theme-on-surface">Lane</native:text>
<native:text ref="land-lane" class="font-mono text-base text-theme-on-surface">{{ (string) ($approval['lane'] ?? '—') }}</native:text>
<native:text class="text-sm font-bold text-theme-on-surface">Commit</native:text>
<native:text ref="land-commit" class="font-mono text-sm text-theme-on-surface">{{ (string) ($approval['commit'] ?? '') !== '' ? $approval['commit'] : 'Not pinned yet' }}</native:text>
@if ((string) ($approval['base'] ?? '') !== '')
    <native:text class="text-xs text-theme-on-surface-variant">Onto {{ $approval['base'] }}</native:text>
@endif
<native:text ref="land-protected" class="text-sm text-theme-on-surface">Changes protected paths: {{ ActionApprovals::flag($approval, 'protected') }}</native:text>
<native:text ref="land-tracing" class="text-sm text-theme-on-surface">Changes tracing/: {{ ActionApprovals::flag($approval, 'tracing') }}</native:text>
