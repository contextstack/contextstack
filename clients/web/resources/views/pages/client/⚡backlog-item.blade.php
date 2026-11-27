<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Livewire\DirectorPage;
use App\Livewire\Forms\BacklogItemForm;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * One backlog item (CLIENT-API.md "Project backlog and heartbeat"): its content, what blocks it, the work and decisions
 * linked to it, and its history. Edits send only the changed fields against the revision shown; a stale one is
 * refused (409) and the page shows the item as it now is. Quick actions set the status (start, done, drop) or clear
 * the owner blocker. There is no delete: dropping an item keeps its history.
 */
new #[Layout('layouts::client')] #[Title('Backlog item')] class extends DirectorPage
{
    public BacklogItemForm $form;

    #[Locked]
    public string $projectId = '';

    #[Locked]
    public string $itemId = '';

    #[Locked]
    public string $projectName = '';

    /** @var list<array<string, mixed>> */
    #[Locked]
    public array $revisions = [];

    public bool $editing = false;

    #[Locked]
    public string $writeKey = '';

    public function mount(string $project, string $item): void
    {
        $this->projectId = $project;
        $this->itemId = $item;
        $this->writeKey = DirectorClient::newKey();
        $this->refetch();
        abort_if($this->refusedStatus === 404, 404);
        $answer = $this->attempt(fn () => $this->director()->get('projects/'.rawurlencode($this->projectId)));
        $this->projectName = (string) ($answer['project']['name'] ?? $this->projectId);
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get($this->path()));
        if (is_array($answer)) {
            $this->take($answer);
        }
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'backlog_item' && ($change['id'] ?? null) === $this->itemId;
    }

    /**
     * A newer revision arrived on the stream: fetch the history with it. An open edit keeps what was typed; saving it
     * is refused as stale, so nothing written since is overwritten.
     */
    protected function afterChanges(): void
    {
        $held = (int) ($this->item()['revision'] ?? 0);
        $answer = $this->attempt(fn () => $this->director()->get($this->path()));
        if (is_array($answer) && is_array($answer['item'] ?? null) && (int) ($answer['item']['revision'] ?? 0) >= $held) {
            $this->take($answer);
        }
    }

    public function edit(): void
    {
        $item = $this->item();
        if ($item === null) {
            return;
        }
        $this->form->fillFrom($item);
        $this->editing = true;
    }

    public function cancelEdit(): void
    {
        $this->editing = false;
        $this->form->resetValidation();
    }

    public function addBlocker(): void
    {
        $this->form->addBlocker();
    }

    public function removeBlocker(int $index): void
    {
        $this->form->removeBlocker($index);
    }

    public function save(): void
    {
        $item = $this->item();
        if ($item === null) {
            return;
        }
        $changes = $this->form->changesFrom($item);
        if ($changes === []) {
            $this->editing = false;

            return;
        }
        if ($this->send([...$changes, 'revision' => $item['revision']])) {
            $this->editing = false;
        }
    }

    public function setStatus(string $status): void
    {
        $item = $this->item();
        if ($item === null || ! in_array($status, BacklogItemForm::Statuses, true) || ($item['status'] ?? null) === $status) {
            return;
        }
        $this->send(['status' => $status, 'revision' => $item['revision']]);
    }

    /**
     * Remove the owner blockers: the answer the item was waiting on has been given. Other blockers stay.
     */
    public function clearOwnerBlocker(): void
    {
        $item = $this->item();
        if ($item === null) {
            return;
        }
        $remaining = array_values(array_filter((array) ($item['blocked_on'] ?? []), fn ($blocker): bool => is_array($blocker) && ($blocker['kind'] ?? null) !== 'owner'));
        $changes = ['blocked_on' => $remaining, 'revision' => $item['revision']];
        if ($remaining === [] && ($item['status'] ?? null) === 'blocked') {
            $changes['status'] = 'next';
        }
        $this->send($changes);
    }

    /**
     * @return array<string, mixed>|null
     */
    public function item(): ?array
    {
        return $this->held('backlog_item', $this->itemId);
    }

    public function waitsOnOwner(): bool
    {
        return collect((array) ($this->item()['blocked_on'] ?? []))->contains(fn ($blocker): bool => is_array($blocker) && ($blocker['kind'] ?? null) === 'owner');
    }

    /**
     * The fields that changed in each version, newest first: the current item, then each earlier revision.
     *
     * @return list<array{version: array<string, mixed>, changed: list<string>}>
     */
    public function history(): array
    {
        $versions = [...$this->revisions];
        if ($this->item() !== null) {
            $versions[] = $this->item();
        }
        $history = [];
        foreach ($versions as $index => $version) {
            $previous = $versions[$index - 1] ?? null;
            $changed = [];
            if ($previous !== null) {
                foreach (['title', 'body', 'status', 'priority', 'blocked_on', 'blocked_reason', 'stop', 'intent_ids', 'job_ids', 'decision_log_entry_ids'] as $field) {
                    if (($previous[$field] ?? null) !== ($version[$field] ?? null)) {
                        $changed[] = $field;
                    }
                }
            }
            $history[] = ['version' => $version, 'changed' => $changed];
        }

        return array_reverse($history);
    }

    /**
     * Post one update against the revision shown. True when the Director took it.
     *
     * @param  array<string, mixed>  $body
     */
    private function send(array $body): bool
    {
        $stale = __('This item changed since you opened it. It now shows as it is; make your change again if it still applies.');

        try {
            $answer = $this->director()->write($this->path().'/update', $body, $this->writeKey);
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->status === 409 ? $stale : $refused->explain();
            if ($refused->status === 409) {
                $this->writeKey = DirectorClient::newKey();
                $this->refetch();
                $this->editing = false;
                $this->problem = $stale;
            }

            return false;
        } catch (DirectorUnavailable|NotConnected $failure) {
            $this->problem = __('The answer was lost: :why. Trying again is safe; it replays the same request.', ['why' => $failure->getMessage()]);

            return false;
        }

        $this->problem = '';
        $this->writeKey = DirectorClient::newKey();
        if (is_array($answer->body['item'] ?? null)) {
            $this->revisions[] = $this->item() ?? [];
            $this->revisions = array_values(array_filter($this->revisions));
            $this->hold('backlog_item', $this->itemId, $answer->body['item']);
        }

        return true;
    }

    /**
     * @param  array<string, mixed>  $answer
     */
    private function take(array $answer): void
    {
        if (! is_array($answer['item'] ?? null)) {
            return;
        }
        $this->records = [];
        $this->hold('backlog_item', $this->itemId, $answer['item']);
        $this->revisions = array_values(array_filter((array) ($answer['revisions'] ?? []), 'is_array'));
    }

    private function path(): string
    {
        return 'projects/'.rawurlencode($this->projectId).'/backlog/'.rawurlencode($this->itemId);
    }
};
?>

<section class="mx-auto flex w-full max-w-4xl flex-col gap-6" data-test="backlog-item-page"
         x-data="directorStream(@js(route('stream')))">
    @php($item = $this->item())

    <x-client.page-header :back="route('backlog.index', $projectId)" :back-label="__('Backlog · :project', ['project' => $projectName])">
        <span data-test="backlog-item-title">{{ $item['title'] ?? __('Backlog item') }}</span>
        @if ($item)
            <x-slot:status>
                <x-client.backlog-status :status="$item['status'] ?? 'next'" />
            </x-slot:status>
        @endif
        <x-slot:actions>
            @if ($item && ! $editing)
                @capable('edit_backlog')
                @if (in_array($item['status'] ?? 'next', ['next', 'blocked'], true))
                    <flux:button size="sm" wire:click="setStatus('now')" data-test="start-item">{{ __('Start') }}</flux:button>
                @endif
                @if (in_array($item['status'] ?? 'next', ['next', 'now', 'blocked'], true))
                    <flux:button size="sm" icon="check" wire:click="setStatus('done')" data-test="done-item">{{ __('Done') }}</flux:button>
                @else
                    <flux:button size="sm" wire:click="setStatus('next')" data-test="reopen-item">{{ __('Reopen') }}</flux:button>
                @endif
                <flux:button size="sm" icon="pencil-square" wire:click="edit" data-test="edit-item">{{ __('Edit') }}</flux:button>
                @if (($item['status'] ?? null) !== 'dropped')
                    <flux:button size="sm" variant="ghost" wire:click="setStatus('dropped')" wire:confirm="{{ __('Drop this item? It leaves the backlog and keeps its history; you can reopen it.') }}" data-test="drop-item">{{ __('Drop') }}</flux:button>
                @endif
                @endcapable
            @endif
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($item && $editing)
        <form wire:submit="save" class="space-y-5" data-test="backlog-edit">
            <x-client.backlog-fields :blockers="$form->blockers" />
            <div class="flex gap-2">
                <flux:button type="submit" variant="primary" data-test="save-item">{{ __('Save') }}</flux:button>
                <flux:button variant="ghost" wire:click="cancelEdit">{{ __('Cancel') }}</flux:button>
            </div>
        </form>
    @elseif ($item)
        @if ($this->waitsOnOwner())
            <flux:callout icon="hand-raised" color="amber" data-test="waits-on-you">
                <flux:callout.heading>{{ __('This waits on you') }}</flux:callout.heading>
                @if (($item['blocked_reason'] ?? '') !== '')
                    <flux:callout.text>{{ $item['blocked_reason'] }}</flux:callout.text>
                @endif
                @if (! empty($item['stop']))
                    <flux:callout.text>{{ __('The project\'s heartbeat is paused until you clear this.') }}</flux:callout.text>
                @endif
                <x-slot:actions>
                    <flux:button size="sm" wire:click="clearOwnerBlocker" data-test="clear-owner-blocker">{{ __('I\'ve answered: clear it') }}</flux:button>
                </x-slot:actions>
            </flux:callout>
        @endif

        <x-client.section :heading="__('Details')">
            @if (($item['body'] ?? '') !== '')
                <x-client.markdown :text="$item['body']" data-test="backlog-item-body" />
            @else
                <p class="text-sm text-zinc-500">{{ __('No details.') }}</p>
            @endif
        </x-client.section>

        <div class="grid gap-6 sm:grid-cols-2">
            <x-client.section :heading="__('Blocked on')" data-test="backlog-item-blockers">
                @forelse ((array) ($item['blocked_on'] ?? []) as $blocker)
                    <p class="text-sm">
                        @switch($blocker['kind'] ?? '')
                            @case('owner') {{ __('You') }} @break
                            @case('external') {{ __('External: :ref', ['ref' => $blocker['ref'] ?? '']) }} @break
                            @case('item')
                                {{ __('Item') }}
                                <flux:link :href="route('backlog.show', ['project' => $projectId, 'item' => $blocker['ref'] ?? ''])" wire:navigate>{{ $blocker['ref'] ?? '' }}</flux:link>
                                @break
                            @case('job')
                                {{ __('Job') }}
                                <flux:link :href="route('lanes.show', $blocker['ref'] ?? '')" wire:navigate>{{ $blocker['ref'] ?? '' }}</flux:link>
                                @break
                        @endswitch
                    </p>
                @empty
                    <p class="text-sm text-zinc-500">{{ __('Nothing.') }}</p>
                @endforelse
            </x-client.section>

            <x-client.section :heading="__('Linked work')" data-test="backlog-item-links">
                @foreach ((array) ($item['job_ids'] ?? []) as $jobId)
                    <p class="text-sm">{{ __('Lane') }} <flux:link :href="route('lanes.show', $jobId)" wire:navigate>{{ $jobId }}</flux:link></p>
                @endforeach
                @foreach ((array) ($item['intent_ids'] ?? []) as $intentId)
                    <p class="text-sm">{{ __('Task') }} <flux:link :href="route('intents.show', $intentId)" wire:navigate>{{ $intentId }}</flux:link></p>
                @endforeach
                @foreach ((array) ($item['decision_log_entry_ids'] ?? []) as $entryId)
                    <p class="text-sm">{{ __('Decision') }} <flux:link :href="route('decisions.show', ['project' => $projectId, 'entry' => $entryId])" wire:navigate>{{ $entryId }}</flux:link></p>
                @endforeach
                @if (($item['job_ids'] ?? []) === [] && ($item['intent_ids'] ?? []) === [] && ($item['decision_log_entry_ids'] ?? []) === [])
                    <p class="text-sm text-zinc-500">{{ __('None yet.') }}</p>
                @endif
            </x-client.section>
        </div>

        <flux:text class="text-xs">{{ __('Priority :n (lower runs first).', ['n' => $item['priority'] ?? 0]) }}</flux:text>
    @endif

    @if ($item)
        <x-client.section :heading="__('History')" data-test="backlog-item-history">
            <ol class="space-y-2">
                @foreach ($this->history() as $step)
                    <li class="flex flex-wrap items-baseline gap-x-2 text-sm" wire:key="version-{{ $loop->remaining }}">
                        <span class="font-mono text-xs text-zinc-500">{{ __('v:n', ['n' => $loop->remaining + 1]) }}</span>
                        <x-client.time :at="$step['version']['updated_at'] ?? ($step['version']['created_at'] ?? null)" />
                        <span class="text-zinc-600 dark:text-zinc-400">
                            @if ($loop->last)
                                {{ __('created') }}
                            @elseif ($step['changed'] === [])
                                {{ __('saved without changes') }}
                            @else
                                {{ __('changed :fields', ['fields' => implode(', ', array_map(fn (string $field): string => str_replace(['_ids', '_'], ['', ' '], $field), $step['changed']))]) }}
                            @endif
                        </span>
                        @if (is_string($step['version']['updated_by'] ?? null))
                            <span class="text-xs text-zinc-500">{{ __('by :who', ['who' => $step['version']['updated_by']]) }}</span>
                        @endif
                    </li>
                @endforeach
            </ol>
        </x-client.section>
    @endif
</section>
