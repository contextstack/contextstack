<?php

use App\Client\ConversationOrder;
use App\Client\Leads;
use App\Client\ListRecords;
use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Attributes\Url;

/*
 * Conversations, newest first (CLIENT-API.md §11, GET conversations), kept current from the stream.
 */
new #[Layout('layouts::client')] #[Title('Conversations')] class extends DirectorPage
{
    /** Conversations shown at first and added by each "Show more". */
    public const PAGE = 50;

    /** @var list<string> conversations a live lead credential is bound to */
    #[Locked]
    public array $leadIds = [];

    #[Locked]
    public int $limit = self::PAGE;

    /** The archived conversations instead of the others (`GET conversations?archived=1`). */
    #[Url]
    public bool $archived = false;

    /** Said once after a conversation was deleted from its page. */
    #[Locked]
    public string $notice = '';

    #[Locked]
    public bool $more = false;

    public function updatedArchived(): void
    {
        $this->limit = self::PAGE;
        $this->refetch();
    }

    public function showMore(): void
    {
        $this->limit += self::PAGE;
        $this->refetch();
    }

    public function mount(): void
    {
        $this->notice = is_string(session('conversation-notice')) ? session('conversation-notice') : '';
        $this->refetch();
    }

    public function refetch(): void
    {
        // One more than shown tells whether there is more; a Director without `limit` sends them all, cut here.
        $answer = $this->attempt(fn () => $this->director()->get('conversations', ['limit' => $this->limit + 1, ...($this->archived ? ['archived' => '1'] : [])]));
        if ($answer === null) {
            return;
        }

        $conversations = array_values(array_filter((array) ($answer['conversations'] ?? []), fn (mixed $conversation): bool => is_array($conversation) && is_string($conversation['conversation_id'] ?? null)));
        usort($conversations, fn (array $a, array $b): int => strcmp((string) ($b['created_at'] ?? ''), (string) ($a['created_at'] ?? '')));
        $this->more = count($conversations) > $this->limit;
        $conversations = array_slice($conversations, 0, $this->limit);

        $this->leadIds = Leads::among($conversations);
        $this->records = [];
        foreach ($conversations as $conversation) {
            $this->hold('conversation', $conversation['conversation_id'], $conversation);
        }
    }

    protected function shown(string $kind, array $record): array
    {
        return ListRecords::of($kind, $record);
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'conversation';
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function conversations(): array
    {
        // A conversation archived or unarchived while the list is open moves to the other view.
        $shown = array_values(array_filter($this->heldOf('conversation'),
            fn (array $conversation): bool => filled($conversation['archived_at'] ?? null) === $this->archived));

        return ConversationOrder::sort($shown, $this->leadIds);
    }
};
?>

<section class="mx-auto flex w-full max-w-5xl flex-col gap-5" data-test="conversations-page"
         x-data="directorStream(@js(route('stream')))">
    <header class="flex items-start justify-between gap-4 max-sm:flex-col max-sm:gap-3">
        <div>
            <flux:heading size="lg" level="1" class="text-lg!">{{ __('Conversations') }}</flux:heading>
            <flux:text class="mt-0.5">{{ __('Threads of messages and replies. Each message is an ordinary job on the conversation\'s machine.') }}</flux:text>
        </div>
        <div class="flex items-center gap-3">
            <x-client.stream-dot />
            @capable('start_work')
            <flux:button variant="primary" icon="plus" :href="route('conversations.create')" wire:navigate>{{ __('New conversation') }}</flux:button>
            @endcapable
        </div>
    </header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif
    @if ($notice !== '')
        <flux:callout variant="success" icon="check-circle" data-test="notice">
            <flux:callout.text>{{ $notice }}</flux:callout.text>
        </flux:callout>
    @endif

    <div class="flex w-fit gap-1 rounded-lg bg-black/[.04] p-0.5 dark:bg-white/[.06]" data-test="archived-switch">
        <button type="button" wire:click="$set('archived', false)" aria-pressed="{{ $archived ? 'false' : 'true' }}" data-test="show-active"
                @class(['rounded-md px-3 py-1 text-sm', 'bg-white text-zinc-900 shadow-sm dark:bg-white/10 dark:text-white' => ! $archived, 'text-zinc-500 hover:text-zinc-900 dark:hover:text-white' => $archived])>{{ __('Active') }}</button>
        <button type="button" wire:click="$set('archived', true)" aria-pressed="{{ $archived ? 'true' : 'false' }}" data-test="show-archived"
                @class(['rounded-md px-3 py-1 text-sm', 'bg-white text-zinc-900 shadow-sm dark:bg-white/10 dark:text-white' => $archived, 'text-zinc-500 hover:text-zinc-900 dark:hover:text-white' => ! $archived])>{{ __('Archived') }}</button>
    </div>

    @php($conversations = $this->conversations())
    @if ($conversations === [])
        <flux:text data-test="no-conversations">{{ $archived ? __('No archived conversations.') : __('No conversations yet. Start one to talk to a spec on a machine.') }}</flux:text>
    @else
        <flux:table>
            <flux:table.columns>
                <flux:table.column>{{ __('Conversation') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Machine') }}</flux:table.column>
                <flux:table.column>{{ __('State') }}</flux:table.column>
                <flux:table.column>{{ __('Messages') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Budget') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Started') }}</flux:table.column>
            </flux:table.columns>
            <flux:table.rows>
                @foreach ($conversations as $conversation)
                    <flux:table.row wire:key="conversation-{{ $conversation['conversation_id'] }}" data-test="conversation-row">
                        <flux:table.cell>
                            <div class="flex flex-col gap-2">
                                <div class="flex flex-wrap items-center gap-2">
                                    <flux:link :href="route('conversations.show', $conversation['conversation_id'])" wire:navigate>
                                        {{ \App\Client\ConversationOrder::name($conversation) }}
                                    </flux:link>
                                    @if (in_array($conversation['conversation_id'], $leadIds, true))
                                        <flux:badge size="sm" icon="wrench-screwdriver" data-test="lead-badge">{{ __('Runs tasks') }}</flux:badge>
                                    @endif
                                </div>
                                <x-client.conversation-needs-you :conversation="$conversation" compact />
                            </div>
                        </flux:table.cell>
                        <flux:table.cell class="max-sm:hidden font-mono">{{ $conversation['node'] }}</flux:table.cell>
                        <flux:table.cell><x-client.conversation-state :conversation="$conversation" /></flux:table.cell>
                        <flux:table.cell>
                            {{ $conversation['turns'] ?? 0 }}
                            @if (count((array) ($conversation['queued_turn_ids'] ?? [])) > 0)
                                <span class="text-zinc-500">({{ trans_choice(':count queued|:count queued', count($conversation['queued_turn_ids'])) }})</span>
                            @endif
                        </flux:table.cell>
                        <flux:table.cell class="max-sm:hidden">{{ $conversation['budget']['shown'] ?? __('no budget set') }}</flux:table.cell>
                        <flux:table.cell class="max-sm:hidden"><x-client.time :at="$conversation['created_at']" /></flux:table.cell>
                    </flux:table.row>
                @endforeach
            </flux:table.rows>
        </flux:table>
        @if ($more)
            <div class="flex justify-center">
                <flux:button size="sm" variant="ghost" wire:click="showMore" data-test="show-more">{{ __('Show more') }}</flux:button>
            </div>
        @endif
    @endif
</section>
