<?php

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Identity;
use App\Livewire\DirectorPage;
use Flux\Flux;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Which conversations may send each other briefs (CLIENT-API.md §11.6, Peer messages): the owner's allowlist of
 * directed pairs. Each direction is its own choice; the whole list is saved at once, and an empty list turns peer
 * messages off. Only active (unarchived) conversations can be chosen.
 *
 * A saved pair can go stale when one of its conversations is archived or deleted; the Director refuses a list that
 * names one (422), so stale pairs are shown as such and left out of every save. The list holds at most 500
 * conversations, so a pair naming one outside it is looked up by itself before it is called stale.
 */
new #[Layout('layouts::client')] #[Title('Messages between conversations')] class extends DirectorPage
{
    /** @var list<array{from_conversation: string, to_conversation: string}> */
    #[Locked]
    public array $pairs = [];

    /** @var array<string, string> conversation id => title, active conversations only */
    #[Locked]
    public array $conversations = [];

    /** @var list<string> conversation ids in saved pairs that are archived or gone */
    #[Locked]
    public array $stale = [];

    #[Locked]
    public bool $allowed = true;

    public string $from = '';

    public string $to = '';

    public string $direction = 'one';

    public function mount(): void
    {
        $this->refetch();
    }

    public function refetch(): void
    {
        $identity = app(Identity::class);
        $this->allowed = $identity->legacy() ? (bool) ($identity->me()['owner'] ?? false) : $identity->can('manage_org_settings');
        if (! $this->allowed) {
            $this->pairs = [];

            return;
        }

        $answer = $this->attempt(fn () => $this->director()->get('peer-message-allowlist'));
        if ($answer === null) {
            return;
        }
        $this->pairs = self::pairsFrom($answer['pairs'] ?? []);

        $list = $this->attempt(fn () => $this->director()->get('conversations', ['limit' => 500])) ?? [];
        $this->conversations = [];
        foreach ((array) ($list['conversations'] ?? []) as $conversation) {
            if (is_array($conversation) && is_string($conversation['conversation_id'] ?? null) && ! filled($conversation['archived_at'] ?? null)) {
                $title = (string) ($conversation['title'] ?? '');
                $this->conversations[$conversation['conversation_id']] = $title !== '' ? $title : $conversation['conversation_id'];
            }
        }
        $this->findStale();
    }

    /**
     * Pair conversations not in the active list: fetch each; archived or gone (404) is stale.
     */
    private function findStale(): void
    {
        $this->stale = [];
        $named = array_unique(array_merge(array_column($this->pairs, 'from_conversation'), array_column($this->pairs, 'to_conversation')));
        foreach (array_diff($named, array_keys($this->conversations)) as $id) {
            try {
                $conversation = $this->director()->find('conversations/'.rawurlencode($id))['conversation'] ?? null;
            } catch (DirectorRefused|DirectorUnavailable) {
                // Unknown for now: neither shown as stale nor dropped from a save.
                continue;
            }
            if (is_array($conversation) && ! filled($conversation['archived_at'] ?? null)) {
                $title = (string) ($conversation['title'] ?? '');
                $this->conversations[$id] = $title !== '' ? $title : $id;
            } else {
                $this->stale[] = $id;
            }
        }
    }

    /**
     * @param  array{from_conversation: string, to_conversation: string}  $pair
     */
    public function isStale(array $pair): bool
    {
        return in_array($pair['from_conversation'], $this->stale, true) || in_array($pair['to_conversation'], $this->stale, true);
    }

    protected function follows(array $change): bool
    {
        return false;
    }

    public function add(): void
    {
        $ids = implode(',', array_keys($this->conversations) ?: ['']);
        $this->validate([
            'from' => ['required', 'in:'.$ids],
            'to' => ['required', 'in:'.$ids, 'different:from'],
            'direction' => ['required', 'in:one,both'],
        ]);

        $pairs = [...$this->pairs, ['from_conversation' => $this->from, 'to_conversation' => $this->to]];
        if ($this->direction === 'both') {
            $pairs[] = ['from_conversation' => $this->to, 'to_conversation' => $this->from];
        }
        $this->save($pairs);
        $this->from = $this->to = '';
    }

    public function remove(int $index): void
    {
        $pairs = $this->pairs;
        unset($pairs[$index]);
        $this->save(array_values($pairs));
    }

    public function turnOff(): void
    {
        $this->save([]);
    }

    public function title(string $id): string
    {
        return $this->conversations[$id] ?? $id;
    }

    /**
     * @param  list<array{from_conversation: string, to_conversation: string}>  $pairs
     */
    private function save(array $pairs): void
    {
        $unique = [];
        foreach (array_filter($pairs, fn (array $pair): bool => ! $this->isStale($pair)) as $pair) {
            $unique[$pair['from_conversation']."\0".$pair['to_conversation']] = $pair;
        }

        $answer = $this->attempt(fn () => $this->director()->replace('peer-message-allowlist', ['pairs' => array_values($unique)]));
        if ($answer === null) {
            return;
        }
        $this->pairs = self::pairsFrom($answer['pairs'] ?? []);
        $this->findStale();
        Flux::toast(variant: 'success', text: $this->pairs === [] ? __('Messages between conversations are off.') : __('Saved.'));
    }

    /**
     * @return list<array{from_conversation: string, to_conversation: string}>
     */
    private static function pairsFrom(mixed $pairs): array
    {
        $found = [];
        foreach (is_array($pairs) ? $pairs : [] as $pair) {
            if (is_array($pair) && is_string($pair['from_conversation'] ?? null) && is_string($pair['to_conversation'] ?? null)) {
                $found[] = ['from_conversation' => $pair['from_conversation'], 'to_conversation' => $pair['to_conversation']];
            }
        }

        return $found;
    }
}; ?>

<section class="mx-auto flex w-full max-w-3xl flex-col gap-5" data-test="peer-messages-page">
    <div>
        <flux:heading size="lg" level="1" class="text-lg!">{{ __('Messages between conversations') }}</flux:heading>
        <flux:text class="mt-0.5">{{ __('A conversation can send a brief to another only in a direction you allow here. Briefs arrive as untrusted text: never instructions or approvals. With nothing allowed, this is off.') }}</flux:text>
    </div>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if (! $allowed)
        <flux:callout icon="information-circle" data-test="peer-not-allowed">
            <flux:callout.text>{{ __('Only the owner chooses which conversations may message each other.') }}</flux:callout.text>
        </flux:callout>
    @elseif (! $signedOut)
        <div class="flex flex-col gap-2" data-test="peer-pairs">
            @forelse ($pairs as $index => $pair)
                <div class="flex items-center justify-between gap-3 rounded-lg border border-zinc-200 px-4 py-2.5 text-sm dark:border-zinc-700" wire:key="pair-{{ $pair['from_conversation'] }}-{{ $pair['to_conversation'] }}" data-test="peer-pair">
                    <span class="min-w-0 truncate">{{ $this->title($pair['from_conversation']) }} → {{ $this->title($pair['to_conversation']) }}</span>
                    @if ($this->isStale($pair))
                        <flux:badge size="sm" color="zinc" data-test="peer-pair-stale">{{ __('archived or gone: dropped at the next save') }}</flux:badge>
                    @endif
                    <flux:button size="xs" variant="ghost" wire:click="remove({{ $index }})">{{ __('Remove') }}</flux:button>
                </div>
            @empty
                <flux:text data-test="peer-off">{{ __('Off: no conversation may message another.') }}</flux:text>
            @endforelse
            @if ($pairs !== [])
                <div><flux:button size="sm" variant="ghost" wire:click="turnOff" data-test="peer-turn-off">{{ __('Turn all off') }}</flux:button></div>
            @endif
        </div>

        <flux:card>
            <form wire:submit="add" class="flex flex-wrap items-end gap-3">
                <flux:select wire:model="from" :label="__('From')" :placeholder="__('Choose…')" class="max-w-56">
                    @foreach ($conversations as $id => $title)
                        <flux:select.option :value="$id" wire:key="from-{{ $id }}">{{ $title }}</flux:select.option>
                    @endforeach
                </flux:select>
                <flux:select wire:model="to" :label="__('To')" :placeholder="__('Choose…')" class="max-w-56">
                    @foreach ($conversations as $id => $title)
                        <flux:select.option :value="$id" wire:key="to-{{ $id }}">{{ $title }}</flux:select.option>
                    @endforeach
                </flux:select>
                <flux:select wire:model="direction" :label="__('Direction')" class="max-w-48">
                    <flux:select.option value="one">{{ __('This direction only') }}</flux:select.option>
                    <flux:select.option value="both">{{ __('Both directions') }}</flux:select.option>
                </flux:select>
                <flux:button type="submit" variant="primary">{{ __('Allow') }}</flux:button>
            </form>
        </flux:card>
    @endif
</section>
