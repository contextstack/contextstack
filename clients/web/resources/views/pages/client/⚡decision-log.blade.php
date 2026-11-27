<?php

use App\Client\DirectorClient;
use App\Livewire\DirectorPage;
use App\Livewire\Forms\DecisionForm;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Attributes\Url;

/*
 * A project's decision log (CLIENT-API.md "Project decision log"): each decision with why, the alternatives considered,
 * when to reopen it and its sources, searched and filtered by the Director, in decision-number order. New decisions
 * are recorded here; an entry's own page revises, supersedes or retires it. Changes arrive on the event stream as
 * kind `decision_log`; since the list is a filtered query, a change to this project's log fetches it again.
 */
new #[Layout('layouts::client')] #[Title('Decision log')] class extends DirectorPage
{
    public DecisionForm $form;

    #[Locked]
    public string $projectId = '';

    #[Locked]
    public string $projectName = '';

    #[Url(as: 'status', except: 'active')]
    public string $status = 'active';

    #[Url(as: 'q', except: '')]
    public string $search = '';

    #[Locked]
    public string $createKey = '';

    public function mount(string $project): void
    {
        $this->projectId = $project;
        $this->createKey = DirectorClient::newKey();
        if (! in_array($this->status, ['active', 'superseded', 'retired', 'all'], true)) {
            $this->status = 'active';
        }
        $answer = $this->attempt(fn () => $this->director()->get('projects/'.rawurlencode($this->projectId)));
        abort_if($this->refusedStatus === 404, 404);
        $this->projectName = (string) ($answer['project']['name'] ?? $this->projectId);
        $this->refetch();
    }

    public function refetch(): void
    {
        $query = array_filter([
            'status' => $this->status === 'all' ? null : $this->status,
            'q' => trim($this->search) !== '' ? trim($this->search) : null,
        ]);
        $answer = $this->attempt(fn () => $this->director()->get($this->base(), $query));
        if (! is_array($answer)) {
            return;
        }
        $this->records = [];
        foreach ((array) ($answer['entries'] ?? []) as $entry) {
            if (is_array($entry) && is_string($entry['entry_id'] ?? null)) {
                $this->hold('decision_log', $entry['entry_id'], $entry);
            }
        }
    }

    public function updatedStatus(): void
    {
        if (! in_array($this->status, ['active', 'superseded', 'retired', 'all'], true)) {
            $this->status = 'active';
        }
        $this->refetch();
    }

    public function updatedSearch(): void
    {
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
        return ($change['kind'] ?? null) === 'decision_log'
            && (($change['record']['project_id'] ?? null) === $this->projectId || isset($this->records['decision_log:'.($change['id'] ?? '')]));
    }

    public function newDecision(): void
    {
        $this->form->reset();
        $this->form->resetValidation();
        $this->modal('new-decision')->show();
    }

    public function addAlternative(): void
    {
        $this->form->addAlternative();
    }

    public function removeAlternative(int $index): void
    {
        $this->form->removeAlternative($index);
    }

    public function create(): void
    {
        $content = $this->form->content();
        $answer = $this->attempt(fn () => $this->director()->write($this->base(), $content, $this->createKey));
        if ($answer === null) {
            return;
        }
        $this->createKey = DirectorClient::newKey();
        $this->modal('new-decision')->close();
        $entryId = (string) ($answer->body['entry']['entry_id'] ?? '');
        if ($entryId !== '') {
            $this->redirectRoute('decisions.show', ['project' => $this->projectId, 'entry' => $entryId], navigate: true);

            return;
        }
        $this->refetch();
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function entries(): array
    {
        $entries = $this->heldOf('decision_log');
        usort($entries, fn (array $a, array $b): int => (int) substr((string) ($a['number'] ?? 'D0'), 1) <=> (int) substr((string) ($b['number'] ?? 'D0'), 1));

        return $entries;
    }

    private function base(): string
    {
        return 'projects/'.rawurlencode($this->projectId).'/decision_log';
    }
};
?>

<section class="mx-auto flex w-full max-w-4xl flex-col gap-6" data-test="decision-log-page"
         x-data="directorStream(@js(route('stream')))">
    <x-client.page-header :back="route('projects.show', $projectId)" :back-label="$projectName">
        {{ __('Decision log') }}
        <x-slot:actions>
            @capable('edit_decision_log')
            <flux:button size="sm" variant="primary" icon="plus" wire:click="newDecision" data-test="new-decision">{{ __('New decision') }}</flux:button>
            @endcapable
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    <flux:text class="-mt-3">{{ __('What this project decided, why, what else was considered, and what would reopen it. A task lane can carry the active log as context.') }}</flux:text>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    <div class="flex flex-wrap items-end gap-3">
        <flux:input wire:model.live.debounce.300ms="search" icon="magnifying-glass" :placeholder="__('Search decisions, reasons and alternatives')" class="min-w-0 flex-1" data-test="decision-search" />
        <flux:select wire:model.live="status" class="w-40" :aria-label="__('Status')" data-test="decision-status">
            <flux:select.option value="active">{{ __('Active') }}</flux:select.option>
            <flux:select.option value="superseded">{{ __('Superseded') }}</flux:select.option>
            <flux:select.option value="retired">{{ __('Retired') }}</flux:select.option>
            <flux:select.option value="all">{{ __('All') }}</flux:select.option>
        </flux:select>
    </div>

    <ul class="divide-y divide-black/5 dark:divide-white/5" data-test="decision-entries">
        @forelse ($this->entries() as $entry)
            @php($alternatives = count((array) ($entry['alternatives'] ?? [])))
            <li class="py-3" wire:key="entry-{{ $entry['entry_id'] }}" data-test="decision-entry">
                <a href="{{ route('decisions.show', ['project' => $projectId, 'entry' => $entry['entry_id']]) }}" wire:navigate class="group block space-y-1">
                    <div class="flex items-baseline gap-3">
                        <span class="w-10 shrink-0 font-mono text-xs text-zinc-500" data-test="decision-number">{{ $entry['number'] }}</span>
                        <span class="min-w-0 flex-1 text-sm font-medium group-hover:underline" data-test="decision-text">{{ $entry['decision'] }}</span>
                        @if (($entry['status'] ?? 'active') !== 'active')
                            <flux:badge size="sm" data-test="decision-status-badge">{{ $entry['status'] }}</flux:badge>
                        @endif
                    </div>
                    <p class="line-clamp-2 ps-13 text-sm text-zinc-600 dark:text-zinc-400">{{ $entry['why'] ?? '' }}</p>
                    <p class="ps-13 text-xs text-zinc-500">
                        @if ($alternatives === 0)
                            <span class="text-amber-600 dark:text-amber-400" data-test="no-alternatives">{{ __('No alternatives recorded') }}</span>
                        @else
                            {{ trans_choice(':count alternative|:count alternatives', $alternatives) }}
                        @endif
                        @if (($entry['reopen_when'] ?? []) !== [])
                            · {{ trans_choice(':count reopen condition|:count reopen conditions', count((array) $entry['reopen_when'])) }}
                        @endif
                    </p>
                </a>
            </li>
        @empty
            <li class="py-3 text-sm text-zinc-500" data-test="no-decisions">
                {{ trim($search) !== '' ? __('No decisions match.') : __('No decisions here yet.') }}
            </li>
        @endforelse
    </ul>

    <flux:modal name="new-decision" class="w-full max-w-2xl">
        <form wire:submit="create" class="space-y-5">
            <flux:heading size="lg">{{ __('New decision') }}</flux:heading>
            <x-client.decision-fields :alternatives="$form->alternatives" />
            <div class="flex justify-end gap-2">
                <flux:modal.close>
                    <flux:button variant="ghost">{{ __('Cancel') }}</flux:button>
                </flux:modal.close>
                <flux:button type="submit" variant="primary" data-test="create-decision">{{ __('Record decision') }}</flux:button>
            </div>
        </form>
    </flux:modal>
</section>
