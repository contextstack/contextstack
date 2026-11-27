<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Livewire\DirectorPage;
use App\Livewire\Forms\DecisionForm;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * One decision log entry (CLIENT-API.md "Project decision log"): its content, its history (earlier revisions, oldest
 * first), and the writes: revise (only what changed, against the revision shown), supersede by another active entry,
 * retire. Every write names the revision it was made against; a stale one is refused (409) and the page shows the
 * entry as it now is. A closed entry cannot be changed, and there is no delete.
 */
new #[Layout('layouts::client')] #[Title('Decision')] class extends DirectorPage
{
    public DecisionForm $form;

    #[Locked]
    public string $projectId = '';

    #[Locked]
    public string $entryId = '';

    #[Locked]
    public string $projectName = '';

    /** @var list<array<string, mixed>> */
    #[Locked]
    public array $revisions = [];

    /** @var array<string, string> entry id => "D3 · decision", other active entries */
    #[Locked]
    public array $replacements = [];

    public bool $editing = false;

    public string $supersededBy = '';

    #[Locked]
    public string $writeKey = '';

    public function mount(string $project, string $entry): void
    {
        $this->projectId = $project;
        $this->entryId = $entry;
        $this->writeKey = DirectorClient::newKey();
        $this->refetch();
        abort_if($this->refusedStatus === 404, 404);
        $answer = $this->attempt(fn () => $this->director()->get('projects/'.rawurlencode($this->projectId)));
        $this->projectName = (string) ($answer['project']['name'] ?? $this->projectId);
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get($this->path()));
        if (! is_array($answer) || ! is_array($answer['entry'] ?? null)) {
            return;
        }
        $this->take($answer);
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'decision_log' && ($change['id'] ?? null) === $this->entryId;
    }

    /**
     * A newer revision arrived on the stream: fetch the history with it. An open edit keeps what was typed; saving it
     * is refused as stale, so nothing written since is overwritten.
     */
    protected function afterChanges(): void
    {
        $held = (int) ($this->entry()['revision'] ?? 0);
        $answer = $this->attempt(fn () => $this->director()->get($this->path()));
        if (is_array($answer) && is_array($answer['entry'] ?? null) && (int) ($answer['entry']['revision'] ?? 0) >= $held) {
            $this->take($answer);
        }
    }

    public function edit(): void
    {
        $entry = $this->entry();
        if ($entry === null || ($entry['status'] ?? null) !== 'active') {
            return;
        }
        $this->form->fillFrom($entry);
        $this->editing = true;
    }

    public function cancelEdit(): void
    {
        $this->editing = false;
        $this->form->resetValidation();
    }

    public function addAlternative(): void
    {
        $this->form->addAlternative();
    }

    public function removeAlternative(int $index): void
    {
        $this->form->removeAlternative($index);
    }

    public function save(): void
    {
        $entry = $this->entry();
        if ($entry === null) {
            return;
        }
        $changes = $this->form->changesFrom($entry);
        if ($changes === []) {
            $this->editing = false;

            return;
        }
        if ($this->send('revise', [...$changes, 'revision' => $entry['revision']])) {
            $this->editing = false;
        }
    }

    public function supersede(): void
    {
        $entry = $this->entry();
        $this->validate(['supersededBy' => ['required', 'string', 'in:'.implode(',', array_keys($this->replacements))]],
            ['supersededBy.required' => __('Choose the decision that replaces this one.')]);
        if ($entry !== null && $this->send('supersede', ['revision' => $entry['revision'], 'superseded_by' => $this->supersededBy])) {
            $this->modal('supersede-decision')->close();
        }
    }

    public function retire(): void
    {
        $entry = $this->entry();
        if ($entry !== null && $this->send('retire', ['revision' => $entry['revision']])) {
            $this->modal('retire-decision')->close();
        }
    }

    /**
     * @return array<string, mixed>|null
     */
    public function entry(): ?array
    {
        return $this->held('decision_log', $this->entryId);
    }

    /**
     * The fields that changed in each version, newest first: the current entry, then each earlier revision.
     *
     * @return list<array{version: array<string, mixed>, changed: list<string>}>
     */
    public function history(): array
    {
        $versions = [...$this->revisions];
        if ($this->entry() !== null) {
            $versions[] = $this->entry();
        }
        $history = [];
        foreach ($versions as $index => $version) {
            $previous = $versions[$index - 1] ?? null;
            $changed = [];
            if ($previous !== null) {
                foreach (['decision', 'why', 'alternatives', 'reopen_when', 'sources', 'status', 'superseded_by'] as $field) {
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
     * Post one write against the revision shown. True when the Director took it.
     *
     * @param  array<string, mixed>  $body
     */
    private function send(string $action, array $body): bool
    {
        try {
            $answer = $this->director()->write($this->path().'/'.$action, $body, $this->writeKey);
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->status === 409
                ? __('This decision changed since you opened it, or it is closed. It now shows as it is; make your change again if it still applies.')
                : $refused->explain();
            if ($refused->status === 409) {
                $this->writeKey = DirectorClient::newKey();
                $answer = $this->attempt(fn () => $this->director()->get($this->path()));
                if (is_array($answer)) {
                    $this->take($answer);
                    $this->editing = false;
                    $this->problem = __('This decision changed since you opened it, or it is closed. It now shows as it is; make your change again if it still applies.');
                }
            }

            return false;
        } catch (DirectorUnavailable|NotConnected $failure) {
            $this->problem = __('The answer was lost: :why. Trying again is safe; it replays the same request.', ['why' => $failure->getMessage()]);

            return false;
        }

        $this->problem = '';
        $this->writeKey = DirectorClient::newKey();
        $this->take($answer->body);

        return true;
    }

    /**
     * @param  array<string, mixed>  $answer
     */
    private function take(array $answer): void
    {
        if (! is_array($answer['entry'] ?? null)) {
            return;
        }
        $this->records = [];
        $this->hold('decision_log', $this->entryId, $answer['entry']);
        $this->revisions = array_values(array_filter((array) ($answer['revisions'] ?? []), 'is_array'));

        $this->replacements = [];
        if (($answer['entry']['status'] ?? null) === 'active' || is_string($answer['entry']['superseded_by'] ?? null)) {
            $list = $this->attempt(fn () => $this->director()->get('projects/'.rawurlencode($this->projectId).'/decision_log', ['status' => 'active']));
            foreach ((array) ($list['entries'] ?? []) as $other) {
                if (is_array($other) && is_string($other['entry_id'] ?? null) && $other['entry_id'] !== $this->entryId) {
                    $this->replacements[$other['entry_id']] = ($other['number'] ?? '').' · '.($other['decision'] ?? '');
                }
            }
        }
    }

    private function path(): string
    {
        return 'projects/'.rawurlencode($this->projectId).'/decision_log/'.rawurlencode($this->entryId);
    }
};
?>

<section class="mx-auto flex w-full max-w-4xl flex-col gap-6" data-test="decision-page"
         x-data="directorStream(@js(route('stream')))">
    @php($entry = $this->entry())

    <x-client.page-header :back="route('decisions.index', $projectId)" :back-label="__('Decision log · :project', ['project' => $projectName])">
        @if ($entry)
            <span class="font-mono text-zinc-500" data-test="decision-number">{{ $entry['number'] }}</span>
            <span data-test="decision-text">{{ $entry['decision'] }}</span>
        @else
            {{ __('Decision') }}
        @endif
        @if ($entry && ($entry['status'] ?? 'active') !== 'active')
            <x-slot:status>
                <flux:badge size="sm" data-test="decision-status">{{ $entry['status'] }}</flux:badge>
            </x-slot:status>
        @endif
        <x-slot:actions>
            @if ($entry && ($entry['status'] ?? null) === 'active' && ! $editing)
                @capable('edit_decision_log')
                <flux:button size="sm" icon="pencil-square" wire:click="edit" data-test="edit-decision">{{ __('Edit') }}</flux:button>
                <flux:dropdown>
                    <flux:button size="sm" variant="ghost" icon="ellipsis-horizontal" square :aria-label="__('More')" />
                    <flux:menu>
                        <flux:modal.trigger name="supersede-decision">
                            <flux:menu.item icon="arrow-uturn-right" data-test="supersede-decision">{{ __('Supersede…') }}</flux:menu.item>
                        </flux:modal.trigger>
                        <flux:modal.trigger name="retire-decision">
                            <flux:menu.item icon="archive-box" data-test="retire-decision">{{ __('Retire…') }}</flux:menu.item>
                        </flux:modal.trigger>
                    </flux:menu>
                </flux:dropdown>
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

    @if ($entry && $editing)
        <form wire:submit="save" class="space-y-5" data-test="decision-edit">
            <x-client.decision-fields :alternatives="$form->alternatives" />
            <div class="flex gap-2">
                <flux:button type="submit" variant="primary" data-test="save-decision">{{ __('Save') }}</flux:button>
                <flux:button variant="ghost" wire:click="cancelEdit">{{ __('Cancel') }}</flux:button>
            </div>
        </form>
    @elseif ($entry)
        @if (is_string($entry['superseded_by'] ?? null))
            <flux:callout icon="arrow-uturn-right" data-test="superseded-by">
                <flux:callout.text>
                    {{ __('Superseded by') }}
                    <flux:link :href="route('decisions.show', ['project' => $projectId, 'entry' => $entry['superseded_by']])" wire:navigate>{{ $replacements[$entry['superseded_by']] ?? $entry['superseded_by'] }}</flux:link>
                </flux:callout.text>
            </flux:callout>
        @endif

        <x-client.section :heading="__('Why')">
            <x-client.markdown :text="$entry['why'] ?? ''" data-test="decision-why" />
        </x-client.section>

        <x-client.section :heading="__('Alternatives considered')" data-test="decision-alternatives">
            @forelse ((array) ($entry['alternatives'] ?? []) as $alternative)
                <div class="border-s-2 border-black/10 ps-3 dark:border-white/10">
                    <p class="text-sm font-medium">{{ $alternative['option'] ?? '' }}</p>
                    <p class="text-sm text-zinc-600 dark:text-zinc-400">{{ $alternative['why_not'] ?? '' }}</p>
                </div>
            @empty
                <p class="text-sm text-zinc-500">
                    {{ __('None recorded yet.') }}
                    @if (($entry['status'] ?? null) === 'active')
                        <flux:link wire:click="edit" class="cursor-pointer" data-test="add-alternatives">{{ __('Add the options you weighed') }}</flux:link>
                    @endif
                </p>
            @endforelse
        </x-client.section>

        <div class="grid gap-6 sm:grid-cols-2">
            <x-client.section :heading="__('Reopen when')" data-test="decision-reopen">
                @forelse ((array) ($entry['reopen_when'] ?? []) as $condition)
                    <p class="text-sm">{{ $condition }}</p>
                @empty
                    <p class="text-sm text-zinc-500">{{ __('No condition recorded.') }}</p>
                @endforelse
            </x-client.section>
            <x-client.section :heading="__('Sources')" data-test="decision-sources">
                @forelse ((array) ($entry['sources'] ?? []) as $source)
                    <p class="text-sm">{{ $source }}</p>
                @empty
                    <p class="text-sm text-zinc-500">{{ __('None recorded.') }}</p>
                @endforelse
            </x-client.section>
        </div>
    @endif

    @if ($entry)
        <x-client.section :heading="__('History')" data-test="decision-history">
            <ol class="space-y-2">
                @foreach ($this->history() as $step)
                    <li class="flex flex-wrap items-baseline gap-x-2 text-sm" wire:key="version-{{ $loop->remaining }}">
                        <span class="font-mono text-xs text-zinc-500">{{ __('v:n', ['n' => $loop->remaining + 1]) }}</span>
                        <x-client.time :at="$step['version']['updated_at'] ?? ($step['version']['created_at'] ?? null)" />
                        <span class="text-zinc-600 dark:text-zinc-400">
                            @if ($loop->last)
                                {{ __('recorded') }}
                            @elseif ($step['changed'] === [])
                                {{ __('saved without changes') }}
                            @else
                                {{ __('changed :fields', ['fields' => implode(', ', array_map(fn (string $field): string => str_replace('_', ' ', $field), $step['changed']))]) }}
                            @endif
                        </span>
                        @if ($loop->first)
                            <span class="text-xs text-zinc-500">{{ __('(current)') }}</span>
                        @endif
                    </li>
                @endforeach
            </ol>
        </x-client.section>
    @endif

    <flux:modal name="supersede-decision" class="w-full max-w-lg">
        <form wire:submit="supersede" class="space-y-4">
            <flux:heading size="lg">{{ __('Supersede this decision') }}</flux:heading>
            <flux:text>{{ __('Name the active decision that replaces it. To record a new one first, add it in the log.') }}</flux:text>
            <flux:select wire:model="supersededBy" :label="__('Replaced by')" :placeholder="__('Choose a decision')" data-test="superseded-by-select">
                @foreach ($replacements as $id => $label)
                    <flux:select.option :value="$id" wire:key="replacement-{{ $id }}">{{ $label }}</flux:select.option>
                @endforeach
            </flux:select>
            <div class="flex justify-end gap-2">
                <flux:modal.close><flux:button variant="ghost">{{ __('Cancel') }}</flux:button></flux:modal.close>
                <flux:button type="submit" variant="primary" data-test="confirm-supersede">{{ __('Supersede') }}</flux:button>
            </div>
        </form>
    </flux:modal>

    <flux:modal name="retire-decision" class="w-full max-w-lg">
        <div class="space-y-4">
            <flux:heading size="lg">{{ __('Retire this decision?') }}</flux:heading>
            <flux:text>{{ __('It closes without a replacement and stays in the log as retired. It cannot be edited afterwards.') }}</flux:text>
            <div class="flex justify-end gap-2">
                <flux:modal.close><flux:button variant="ghost">{{ __('Cancel') }}</flux:button></flux:modal.close>
                <flux:button variant="danger" wire:click="retire" data-test="confirm-retire">{{ __('Retire') }}</flux:button>
            </div>
        </div>
    </flux:modal>
</section>
