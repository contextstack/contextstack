<?php

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Projects (CLIENT-API.md §12.1): each groups intents and conversations and may carry a budget in dollars of actual
 * pay-as-you-go spending. Create one, with or without a budget, and see each one's budget as the Director shows it.
 */
new #[Layout('layouts::client')] #[Title('Projects')] class extends DirectorPage
{
    public string $name = '';

    public string $budgetUsd = '';

    public string $budgetPeriod = 'total';

    #[Locked]
    public string $createKey = '';

    public function mount(): void
    {
        $this->createKey = DirectorClient::newKey();
        $this->refetch();
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get('projects'));
        if ($answer === null) {
            return;
        }

        $this->records = [];
        foreach ((array) ($answer['projects'] ?? []) as $project) {
            $this->hold('project', (string) $project['project_id'], $project);
        }
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'project';
    }

    public function create(): void
    {
        $this->validate([
            'name' => ['required', 'string', 'max:200'],
            'budgetUsd' => ['nullable', 'numeric', 'min:0', 'max:1000000'],
            'budgetPeriod' => ['required', 'in:total,day,month'],
        ]);

        $body = ['name' => $this->name];
        if ($this->budgetUsd !== '') {
            $body['budget'] = ['usd' => $this->budgetUsd, 'period' => $this->budgetPeriod];
        }

        try {
            $answer = $this->director()->write('projects', $body, $this->createKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Creating it again is safe; it replays the same request.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->createKey = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return;
        }

        $this->problem = '';
        $this->createKey = DirectorClient::newKey();
        $this->reset('name', 'budgetUsd', 'budgetPeriod');
        $this->redirectRoute('projects.show', ['project' => $answer->body['project']['project_id']], navigate: true);
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function projects(): array
    {
        $projects = $this->heldOf('project');
        usort($projects, fn (array $a, array $b): int => strcmp((string) $b['created_at'], (string) $a['created_at']));

        return $projects;
    }
};
?>

<section class="mx-auto flex w-full max-w-5xl flex-col gap-5" data-test="projects-page"
         x-data="directorStream(@js(route('stream')))">
    <header class="flex items-start justify-between gap-4 max-sm:flex-col max-sm:gap-3">
        <div>
            <flux:heading size="lg" level="1" class="text-lg!">{{ __('Projects') }}</flux:heading>
            <flux:text class="mt-0.5">{{ __('A project groups tasks and conversations, and may hold a budget on top of the gateway\'s source budgets.') }}</flux:text>
        </div>
        <x-client.stream-dot />
    </header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @php($projects = $this->projects())
    @if ($projects === [])
        <flux:text data-test="no-projects">{{ __('No projects yet.') }}</flux:text>
    @else
        <flux:table>
            <flux:table.columns>
                <flux:table.column>{{ __('Project') }}</flux:table.column>
                <flux:table.column>{{ __('Budget') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Waiting jobs') }}</flux:table.column>
                <flux:table.column class="max-sm:hidden">{{ __('Created') }}</flux:table.column>
            </flux:table.columns>
            <flux:table.rows>
                @foreach ($projects as $project)
                    <flux:table.row wire:key="project-{{ $project['project_id'] }}" data-test="project-row">
                        <flux:table.cell><flux:link :href="route('projects.show', $project['project_id'])" wire:navigate>{{ $project['name'] }}</flux:link></flux:table.cell>
                        <flux:table.cell><x-client.budget :budget="$project['budget'] ?? null" /></flux:table.cell>
                        <flux:table.cell class="max-sm:hidden">{{ count((array) ($project['budget']['waiting_job_ids'] ?? [])) }}</flux:table.cell>
                        <flux:table.cell class="max-sm:hidden"><x-client.time :at="$project['created_at']" /></flux:table.cell>
                    </flux:table.row>
                @endforeach
            </flux:table.rows>
        </flux:table>
    @endif

    <flux:card>
        <form wire:submit="create" class="space-y-4">
            <flux:heading size="lg">{{ __('New project') }}</flux:heading>
            <flux:input wire:model="name" :label="__('Name')" />
            <div class="flex flex-wrap items-end gap-3">
                <flux:input wire:model="budgetUsd" :label="__('Budget in dollars')" :placeholder="__('none')" type="number" step="0.01" min="0" class="w-40" />
                <flux:select wire:model="budgetPeriod" :label="__('Per')" class="w-36">
                    <flux:select.option value="total">{{ __('total') }}</flux:select.option>
                    <flux:select.option value="day">{{ __('day') }}</flux:select.option>
                    <flux:select.option value="month">{{ __('month') }}</flux:select.option>
                </flux:select>
                <flux:button type="submit" variant="primary">{{ __('Create') }}</flux:button>
            </div>
        </form>
    </flux:card>
</section>
