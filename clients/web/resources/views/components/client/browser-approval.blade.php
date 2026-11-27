@props(['approval', 'needsYou' => false, 'showJob' => true])

{{--
    A browser action approval (CLIENT-API.md "Action approvals", docs/design/browser-action-approvals.md): an agent
    driving a browser asks to submit, post or buy on an HTTPS origin. Shows the origin, the action and the exact
    target as plain text; every captured form value in order (hidden ones labelled, masked ones as their length);
    the screenshot the extension took, through this app's own route so the page never holds the Director credential;
    and when the window ends. Only a human owner decides it. In needs-you, `actions.answer` answers go or no_go;
    otherwise `actions.decide` approves or denies. The host page extends ApprovalsPage (decideApproval,
    answerBrowserApproval).
--}}
@php
    use App\Client\BrowserApproval;

    $id = (string) $approval['id'];
    $status = (string) ($approval['status'] ?? 'needs_owner');
    $waiting = $status === 'needs_owner';
    $action = is_string($approval['action'] ?? null) ? $approval['action'] : '';
    $fields = BrowserApproval::fields($approval);
    $warning = BrowserApproval::fieldWarning($approval);
    $sha = BrowserApproval::screenshotSha($approval);
    $pastExpiry = $waiting && BrowserApproval::pastExpiry($approval);
    $answerable = $needsYou && BrowserApproval::answerable($approval);
    $decidable = ! $answerable && BrowserApproval::decidable($approval);
    $decidedByOwner = filled($approval['decided_by'] ?? null);
    [$label, $color] = match ($status) {
        'needs_owner' => [\App\Client\NeedsYou::assignment($approval), 'amber'],
        'approved' => [__('Approved'), 'green'],
        'denied' => [__('Denied'), 'red'],
        'expired' => [__('Expired'), 'zinc'],
        default => [$status, 'zinc'],
    };
    $verb = match ($action) {
        'submit' => __('submit a form'),
        'post' => __('post'),
        'buy' => __('buy'),
        default => $action,
    };
@endphp

<article id="browser-{{ $id }}" @class([
    'space-y-3 rounded-xl border px-4 py-3',
    'border-amber-500/30 bg-amber-500/[.06]' => $waiting,
    'border-black/[.08] dark:border-white/[.08]' => ! $waiting,
]) data-test="browser-approval" data-approval="{{ $id }}" data-status="{{ $status }}" wire:key="browser-{{ $id }}">
    <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-600 dark:text-zinc-400">
        <flux:icon.globe-alt variant="micro" @class(['text-amber-500' => $waiting]) />
        <span class="font-medium text-zinc-800 dark:text-zinc-200">{{ __('An agent asks to :action in the browser', ['action' => $verb]) }}</span>
        <flux:badge size="sm" :color="$color" data-test="browser-status">{{ $label }}</flux:badge>
        @if (filled($approval['created_at'] ?? null))
            <span>· <x-client.time :at="$approval['created_at']" /></span>
        @endif
    </div>

    <dl class="grid gap-x-4 gap-y-2 text-sm sm:grid-cols-[8rem_minmax(0,1fr)]" data-test="browser-facts">
        <dt class="text-zinc-500">{{ __('Origin') }}</dt>
        <dd class="break-all font-mono text-xs" data-test="browser-origin">{{ $approval['origin'] ?? '' }}</dd>
        <dt class="text-zinc-500">{{ __('Action') }}</dt>
        <dd data-test="browser-action"><flux:badge size="sm" :color="$action === 'buy' ? 'red' : 'zinc'">{{ $action }}</flux:badge></dd>
        <dt class="text-zinc-500">{{ __('Target') }}</dt>
        <dd class="whitespace-pre-wrap break-all font-mono text-xs" data-test="browser-target">{{ $approval['target'] ?? '' }}</dd>
        @if (filled($approval['reason'] ?? null))
            <dt class="text-zinc-500">{{ __('Reason') }}</dt>
            <dd data-test="browser-reason">{{ $approval['reason'] }}</dd>
        @endif
        @if ($showJob && filled($approval['job_id'] ?? null))
            <dt class="text-zinc-500">{{ __('Task lane') }}</dt>
            <dd><flux:link :href="route('lanes.show', $approval['job_id'])" wire:navigate class="font-mono text-xs" data-test="browser-job">{{ $approval['job_id'] }}</flux:link></dd>
        @endif
    </dl>

    <div class="space-y-1">
        <p class="text-xs font-medium text-zinc-600 dark:text-zinc-300">{{ trans_choice(':count field it will send|:count fields it will send', count($fields), ['count' => count($fields)]) }}</p>
        @if ($fields !== [])
            <ol class="divide-y divide-black/[.06] rounded-md border border-black/[.08] text-sm dark:divide-white/[.06] dark:border-white/[.08]" data-test="browser-fields">
                @foreach ($fields as $index => $field)
                    <li class="flex flex-wrap items-baseline gap-x-3 gap-y-1 px-3 py-1.5" data-test="browser-field" data-masked="{{ $field['value'] === null ? 'true' : 'false' }}" data-hidden="{{ $field['hidden'] ? 'true' : 'false' }}" wire:key="browser-{{ $id }}-field-{{ $index }}">
                        <span class="break-all font-mono text-xs font-medium" data-test="browser-field-name">{{ $field['name'] !== '' ? $field['name'] : __('(unnamed)') }}</span>
                        @if ($field['hidden'])
                            <flux:badge size="sm" color="amber" data-test="browser-field-hidden">{{ __('hidden') }}</flux:badge>
                        @endif
                        @if ($field['excluded'])
                            <flux:badge size="sm" data-test="browser-field-excluded">{{ __('excluded (per-load token)') }}</flux:badge>
                        @endif
                        @if ($field['value'] !== null)
                            <span class="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-xs" data-test="browser-field-value">{{ $field['value'] }}</span>
                        @elseif ($field['length'] !== null)
                            <span class="text-xs text-zinc-500" data-test="browser-field-masked">{{ trans_choice('masked, :count character|masked, :count characters', $field['length'], ['count' => $field['length']]) }}</span>
                        @else
                            <span class="text-xs text-zinc-500" data-test="browser-field-masked">{{ __('masked') }}</span>
                        @endif
                    </li>
                @endforeach
            </ol>
        @endif
    </div>

    @if ($sha !== null)
        @php($screenshot = route('approvals.browser-screenshot', $sha))
        <div class="space-y-1" data-test="browser-screenshot">
            <flux:modal.trigger :name="'browser-shot-'.$id">
                <button type="button" class="block overflow-hidden rounded-md border border-black/[.08] dark:border-white/[.08]" data-test="browser-screenshot-open">
                    <img src="{{ $screenshot }}" alt="{{ __('Screenshot the browser took when it proposed this action') }}" loading="lazy" class="max-h-48 max-w-full object-contain object-top" data-test="browser-screenshot-thumb">
                </button>
            </flux:modal.trigger>
            <p class="text-xs text-zinc-500">{{ __('Captured when the action was proposed. Select it to see full size.') }}</p>
        </div>
        <flux:modal :name="'browser-shot-'.$id" class="w-full max-w-6xl" data-test="browser-screenshot-modal">
            <div class="space-y-3">
                <flux:heading size="lg">{{ __('Browser screenshot') }}</flux:heading>
                <flux:text class="break-all font-mono text-xs">{{ $approval['origin'] ?? '' }}</flux:text>
                <img src="{{ $screenshot }}" alt="{{ __('Screenshot the browser took when it proposed this action, full size') }}" class="h-auto max-w-full">
            </div>
        </flux:modal>
    @endif

    @if ($waiting)
        @if ($warning !== null)
            <p role="alert" class="flex items-center gap-2 rounded-md border border-amber-500/60 bg-amber-500/10 px-3 py-2 text-sm font-semibold text-amber-900 dark:text-amber-200" data-test="browser-field-warning">
                <flux:icon.exclamation-triangle variant="micro" />
                {{ trans_choice(':count field masked or hidden — review the screenshot|:count fields masked or hidden — review the screenshot', $warning, ['count' => $warning]) }}
            </p>
        @endif
        @if (filled($approval['expires_at'] ?? null))
            <p class="text-xs text-zinc-600 dark:text-zinc-300" data-test="browser-expires">{{ __('The window to decide ends') }} <x-client.time :at="$approval['expires_at']" />. {{ __('After that it expires and nothing is sent.') }}</p>
        @endif
        @if ($answerable)
            <div class="flex flex-wrap items-center gap-2">
                <flux:button size="sm" variant="primary" icon="check" wire:click="answerBrowserApproval(@js($id), 'go')" data-test="browser-answer-go">{{ __('Go') }}</flux:button>
                <flux:button size="sm" variant="danger" icon="x-mark" wire:click="answerBrowserApproval(@js($id), 'no_go')" data-test="browser-answer-no-go">{{ __('No go') }}</flux:button>
                <span class="text-xs text-zinc-500">{{ __('Go allows this one action, once, with exactly these values.') }}</span>
            </div>
        @elseif ($decidable)
            <div class="flex flex-wrap items-center gap-2">
                <flux:button size="sm" variant="primary" icon="check" wire:click="decideApproval(@js($id), 'approve')" data-test="approve-browser">{{ __('Approve') }}</flux:button>
                <flux:button size="sm" variant="danger" icon="x-mark" wire:click="decideApproval(@js($id), 'deny')" data-test="deny-browser">{{ __('Deny') }}</flux:button>
                <span class="text-xs text-zinc-500">{{ __('Approving allows this one action, once, with exactly these values.') }}</span>
            </div>
        @elseif ($pastExpiry)
            <p class="text-xs text-zinc-500" data-test="browser-window-ended">{{ __('The window to decide has ended by this device\'s clock. The Director marks it expired, and nothing is sent.') }}</p>
        @else
            <p class="text-xs text-zinc-500" data-test="browser-read-only">{{ __('Only an owner decides a browser action.') }}</p>
        @endif
    @else
        <p class="text-xs text-zinc-500" data-test="browser-outcome">
            @if ($status === 'approved')
                {{ __('Approved.') }}
                {{-- The approval record's outcome is authoritative; the follow-up job's state is not. --}}
                @switch(data_get($approval, 'outcome.status'))
                    @case('submitted')
                    @case('succeeded')
                        <span data-test="browser-outcome-submitted">{{ __('The site accepted the submission.') }}</span>
                        @break
                    @case('submission_unknown')
                        <span data-test="browser-outcome-unknown">{{ __('The site received the submission, but the result is unknown. Check the site before trying again.') }}</span>
                        @break
                    @case('refused')
                        <span data-test="browser-outcome-refused">{{ __('The browser refused before sending anything') }}@if (filled(data_get($approval, 'outcome.reason'))): {{ data_get($approval, 'outcome.reason') }}@endif. {{ __('It will be proposed again on the next run.') }}</span>
                        @break
                @endswitch
                @if (filled($approval['followup_job_id'] ?? null))
                    {{-- The follow-up job has no lane of its own; it runs under the proposing job's task. --}}
                    {{ __('The browser carries it out as job') }} <span class="font-mono" data-test="browser-followup-job">{{ $approval['followup_job_id'] }}</span>@if (filled($approval['intent_id'] ?? null)), {{ __('in') }} <flux:link :href="route('intents.show', $approval['intent_id'])" wire:navigate data-test="browser-followup-task">{{ __('its task') }}</flux:link>{{ __(', where its result shows') }}@endif.
                @endif
            @elseif ($status === 'denied')
                {{ __('Denied. Nothing was sent.') }}
            @elseif ($status === 'expired')
                {{ __('The window ended before it was decided, so nothing was sent.') }}
            @endif
            @if ($decidedByOwner && filled($approval['decided_at'] ?? null))
                {{ __('Decided') }} <x-client.time :at="$approval['decided_at']" />.
            @endif
        </p>
    @endif
</article>
