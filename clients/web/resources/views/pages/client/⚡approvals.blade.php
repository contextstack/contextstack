<?php

use App\Livewire\ApprovalsPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Title;

/*
 * Action approvals (CLIENT-API.md "Action approvals"): what waits on the owner first, then what was asked recently.
 * Every `action_approval` change on the stream is fetched again, so a new request appears as it is made.
 */
new #[Layout('layouts::client')] #[Title('Approvals')] class extends ApprovalsPage
{
    /** How many decided or in-progress requests are shown below the waiting ones. */
    public const RECENT = 50;

    public function mount(): void
    {
        $this->refetch();
    }

    public function refetch(): void
    {
        $this->loadApprovals();
    }

    protected function followsApproval(array $change): bool
    {
        return true;
    }

    protected function follows(array $change): bool
    {
        return false;
    }

    /**
     * Everything that does not wait on the owner, newest first.
     *
     * @return list<array<string, mixed>>
     */
    public function recent(): array
    {
        $rest = array_values(array_filter($this->approvals, fn (array $approval): bool => ($approval['status'] ?? null) !== 'needs_owner'));
        usort($rest, fn (array $a, array $b): int => strcmp((string) ($b['created_at'] ?? ''), (string) ($a['created_at'] ?? '')));

        return array_slice($rest, 0, self::RECENT);
    }
};
?>

<section class="mx-auto flex w-full max-w-4xl flex-col gap-6" data-test="approvals-page"
         x-data="directorStream(@js(route('stream')))">
    <x-client.page-header>
        {{ __('Approvals') }}
        <x-slot:actions>
            <x-client.stream-dot />
        </x-slot:actions>
    </x-client.page-header>

    <flux:text class="-mt-3">{{ __('When an agent asks for something beyond its sandbox, a second model reviews it. A no-go, and anything on the hard-line list, waits here for you.') }}</flux:text>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    <x-client.section :heading="__('Waiting on you')" data-test="approvals-waiting">
        <div class="space-y-3">
            @forelse ($this->approvalsWaiting() as $approval)
                <x-client.action-approval :approval="$approval" />
            @empty
                <p class="text-sm text-zinc-500" data-test="approvals-none-waiting">{{ __('Nothing waits on you.') }}</p>
            @endforelse
        </div>
    </x-client.section>

    @if ($this->recent() !== [])
        <x-client.section :heading="__('Recent')" data-test="approvals-recent">
            <div class="space-y-3">
                @foreach ($this->recent() as $approval)
                    <x-client.action-approval :approval="$approval" />
                @endforeach
            </div>
        </x-client.section>
    @endif
</section>
