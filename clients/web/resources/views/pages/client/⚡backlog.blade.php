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
use Livewire\Attributes\Url;

/*
 * A project's backlog (CLIENT-API.md "Project backlog and heartbeat"): what waits on you first, then the items in
 * priority order, filtered by status. Dragging an item reorders the list: the moved items get new priorities in one
 * atomic reorder, each against the revision shown, so a stale list is refused (409) and fetched again. Changes
 * arrive on the event stream as kind `backlog_item`.
 */
new #[Layout('layouts::client')] #[Title('Backlog')] class extends DirectorPage
{
    public const array Filters = ['open', 'next', 'now', 'blocked', 'done', 'dropped', 'all'];

    public BacklogItemForm $form;

    #[Locked]
    public string $projectId = '';

    #[Locked]
    public string $projectName = '';

    #[Url(as: 'status', except: 'open')]
    public string $status = 'open';

    /** @var list<array<string, mixed>> open items with an owner blocker */
    #[Locked]
    public array $needsYou = [];

    /** @var array<string, mixed>|null what the heartbeat or a lead would take next */
    #[Locked]
    public ?array $next = null;

    #[Locked]
    public string $createKey = '';

    public function mount(string $project): void
    {
        $this->projectId = $project;
        $this->createKey = DirectorClient::newKey();
        if (! in_array($this->status, self::Filters, true)) {
            $this->status = 'open';
        }
        $answer = $this->attempt(fn () => $this->director()->get('projects/'.rawurlencode($this->projectId)));
        abort_if($this->refusedStatus === 404, 404);
        $this->projectName = (string) ($answer['project']['name'] ?? $this->projectId);
        $this->refetch();
    }

    public function refetch(): void
    {
        $query = in_array($this->status, ['open', 'all'], true) ? [] : ['status' => $this->status];
        $answer = $this->attempt(fn () => $this->director()->get($this->base(), $query));
        if (! is_array($answer)) {
            return;
        }
        $this->records = [];
        foreach ((array) ($answer['items'] ?? []) as $item) {
            if (is_array($item) && is_string($item['item_id'] ?? null)) {
                $this->hold('backlog_item', $item['item_id'], $item);
            }
        }

        $needsYou = $this->attempt(fn () => $this->director()->get($this->base().'/needs_you'));
        $this->needsYou = array_values(array_filter((array) ($needsYou['items'] ?? []), 'is_array'));
        $next = $this->attempt(fn () => $this->director()->get($this->base().'/next_unblocked'));
        $this->next = is_array($next['item'] ?? null) ? $next['item'] : null;
    }

    public function updatedStatus(): void
    {
        if (! in_array($this->status, self::Filters, true)) {
            $this->status = 'open';
        }
        $this->refetch();
    }

    /**
     * @param  list<mixed>  $changes
     * @param  list<mixed>  $runEvents
     */
    public function applyStream(array $changes, array $runEvents = []): void
    {
        foreach ($changes as $change) {
            if (is_array($change) && $this->follows($change)) {
                $this->refetch();

                return;
            }
        }
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'backlog_item'
            && (($change['record']['project_id'] ?? null) === $this->projectId || isset($this->records['backlog_item:'.($change['id'] ?? '')]));
    }

    /**
     * The items shown, in priority order (then by id, as the Director orders them).
     *
     * @return list<array<string, mixed>>
     */
    public function items(): array
    {
        $items = $this->heldOf('backlog_item');
        if ($this->status === 'open') {
            $items = array_values(array_filter($items, fn (array $item): bool => in_array($item['status'] ?? 'next', ['next', 'now', 'blocked'], true)));
        }
        usort($items, fn (array $a, array $b): int => [(int) ($a['priority'] ?? 0), (string) $a['item_id']] <=> [(int) ($b['priority'] ?? 0), (string) $b['item_id']]);

        return $items;
    }

    /**
     * Drag-and-drop (wire:sort): move one item to a new position in the list shown, then give every shown item whose
     * place changed a new priority, ten apart, in one atomic reorder.
     */
    public function reorder(string $itemId, int $position): void
    {
        $items = $this->items();
        $ids = array_column($items, 'item_id');
        $from = array_search($itemId, $ids, true);
        if ($from === false) {
            return;
        }
        $moved = array_splice($items, $from, 1);
        array_splice($items, max(0, min($position, count($items))), 0, $moved);

        $changes = [];
        foreach ($items as $index => $item) {
            $priority = ($index + 1) * 10;
            if ((int) ($item['priority'] ?? 0) !== $priority) {
                $changes[] = ['item_id' => $item['item_id'], 'revision' => $item['revision'], 'priority' => $priority];
            }
        }
        if ($changes === []) {
            return;
        }

        try {
            $answer = $this->director()->write($this->base().'/reorder', ['items' => $changes], DirectorClient::newKey());
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->status === 409
                ? __('The backlog changed since you opened it. It now shows as it is; drag again if the order still needs changing.')
                : $refused->explain();
            $this->refetch();
            if ($refused->status === 409) {
                $this->problem = __('The backlog changed since you opened it. It now shows as it is; drag again if the order still needs changing.');
            }

            return;
        } catch (DirectorUnavailable|NotConnected $failure) {
            $this->problem = __('The new order was not saved: :why', ['why' => $failure->getMessage()]);

            return;
        }

        $this->problem = '';
        foreach ((array) ($answer->body['items'] ?? []) as $item) {
            if (is_array($item) && is_string($item['item_id'] ?? null)) {
                $this->hold('backlog_item', $item['item_id'], $item);
            }
        }
    }

    public function newItem(): void
    {
        $this->form->reset();
        $this->form->resetValidation();
        $items = $this->items();
        $this->form->priority = $items === [] ? 10 : (int) (end($items)['priority'] ?? 0) + 10;
        $this->modal('new-backlog-item')->show();
    }

    public function addBlocker(): void
    {
        $this->form->addBlocker();
    }

    public function removeBlocker(int $index): void
    {
        $this->form->removeBlocker($index);
    }

    public function create(): void
    {
        $content = $this->form->content();
        $answer = $this->attempt(fn () => $this->director()->write($this->base(), $content, $this->createKey));
        if ($answer === null) {
            return;
        }
        $this->createKey = DirectorClient::newKey();
        $this->modal('new-backlog-item')->close();
        $this->refetch();
    }

    private function base(): string
    {
        return 'projects/'.rawurlencode($this->projectId).'/backlog';
    }
};
?>

<section class="mx-auto flex w-full max-w-4xl flex-col gap-6" data-test="backlog-page"
         x-data="directorStream(@js(route('stream')))">
    <x-client.page-header :back="route('projects.show', $projectId)" :back-label="$projectName">
        {{ __('Backlog') }}
        <x-slot:actions>
            <flux:button size="sm" variant="primary" icon="plus" wire:click="newItem" data-test="new-backlog-item">{{ __('New item') }}</flux:button>
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    <flux:text class="-mt-3">{{ __('What this project should do next, in order. A hosted lead takes the next unblocked item; drag to change the order.') }}</flux:text>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($needsYou !== [])
        <div class="space-y-2" data-test="backlog-needs-you">
            <flux:heading size="lg" level="2">{{ __('Waiting on you') }}</flux:heading>
            <ul class="divide-y divide-amber-500/20 rounded-lg border border-amber-500/30 bg-amber-50/60 dark:bg-amber-950/20">
                @foreach ($needsYou as $item)
                    <li class="px-4 py-3" wire:key="needs-{{ $item['item_id'] }}">
                        <a href="{{ route('backlog.show', ['project' => $projectId, 'item' => $item['item_id']]) }}" wire:navigate class="group block">
                            <div class="flex items-baseline gap-2">
                                <span class="text-sm font-medium group-hover:underline">{{ $item['title'] }}</span>
                                @if (! empty($item['stop']))
                                    <flux:badge size="sm" color="amber">{{ __('heartbeat paused') }}</flux:badge>
                                @endif
                            </div>
                            @if (($item['blocked_reason'] ?? '') !== '')
                                <p class="mt-0.5 text-sm text-zinc-600 dark:text-zinc-400">{{ $item['blocked_reason'] }}</p>
                            @endif
                        </a>
                    </li>
                @endforeach
            </ul>
        </div>
    @endif

    <div class="flex flex-wrap items-center justify-between gap-3">
        <flux:text class="text-sm" data-test="backlog-next">
            @if ($next !== null)
                {{ __('Next up:') }}
                <a href="{{ route('backlog.show', ['project' => $projectId, 'item' => $next['item_id']]) }}" wire:navigate class="font-medium underline">{{ $next['title'] }}</a>
            @else
                {{ __('Nothing is ready to take next.') }}
            @endif
        </flux:text>
        <flux:select wire:model.live="status" class="w-40" :aria-label="__('Status')" data-test="backlog-status">
            <flux:select.option value="open">{{ __('Open') }}</flux:select.option>
            <flux:select.option value="next">{{ __('Next') }}</flux:select.option>
            <flux:select.option value="now">{{ __('Now') }}</flux:select.option>
            <flux:select.option value="blocked">{{ __('Blocked') }}</flux:select.option>
            <flux:select.option value="done">{{ __('Done') }}</flux:select.option>
            <flux:select.option value="dropped">{{ __('Dropped') }}</flux:select.option>
            <flux:select.option value="all">{{ __('All') }}</flux:select.option>
        </flux:select>
    </div>

    <ul class="divide-y divide-black/5 dark:divide-white/5" wire:sort="reorder" data-test="backlog-items">
        @forelse ($this->items() as $item)
            <li class="flex items-start gap-3 py-3" wire:key="item-{{ $item['item_id'] }}" wire:sort:item="{{ $item['item_id'] }}" data-test="backlog-item">
                <flux:icon.bars-2 class="mt-0.5 size-4 shrink-0 cursor-grab text-zinc-400" :aria-label="__('Drag to reorder')" />
                <a href="{{ route('backlog.show', ['project' => $projectId, 'item' => $item['item_id']]) }}" wire:navigate wire:sort:ignore class="group block min-w-0 flex-1 space-y-1">
                    <div class="flex items-baseline gap-2">
                        <span class="min-w-0 flex-1 text-sm font-medium group-hover:underline" data-test="backlog-item-title">{{ $item['title'] }}</span>
                        <x-client.backlog-status :status="$item['status'] ?? 'next'" />
                    </div>
                    @if (($item['blocked_on'] ?? []) !== [])
                        <p class="text-xs text-zinc-500">
                            {{ __('Blocked on') }}
                            {{ collect($item['blocked_on'])->map(fn ($b) => ($b['kind'] ?? '') === 'owner' ? __('you') : ($b['kind'] ?? '').' '.($b['ref'] ?? ''))->implode(', ') }}
                        </p>
                    @endif
                </a>
            </li>
        @empty
            <li class="py-3 text-sm text-zinc-500" data-test="no-backlog-items">{{ __('No items here.') }}</li>
        @endforelse
    </ul>

    <flux:modal name="new-backlog-item" class="w-full max-w-2xl">
        <form wire:submit="create" class="space-y-5">
            <flux:heading size="lg">{{ __('New backlog item') }}</flux:heading>
            <x-client.backlog-fields :blockers="$form->blockers" />
            <div class="flex justify-end gap-2">
                <flux:modal.close>
                    <flux:button variant="ghost">{{ __('Cancel') }}</flux:button>
                </flux:modal.close>
                <flux:button type="submit" variant="primary" data-test="create-backlog-item">{{ __('Add to backlog') }}</flux:button>
            </div>
        </form>
    </flux:modal>
</section>
