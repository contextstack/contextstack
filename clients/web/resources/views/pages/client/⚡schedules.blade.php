<?php

use App\Client\DirectorClient;
use App\Client\Identity;
use App\Client\MemberNames;
use App\Client\PausedReason;
use App\Livewire\DirectorPage;
use Flux\Flux;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Every machine's schedules (CLIENT-API.md §4.4) with the member responsible for each. New firings pause while that
 * member cannot run work (disabled, or a role that cannot); an owner or admin hands the schedule to an active member
 * with `PATCH schedules/{node}/{id}` (keyed). Firings already offered or running still finish.
 */
new #[Layout('layouts::client')] #[Title('Schedules')] class extends DirectorPage
{
    /** @var list<array{id: string, display_name: string, role: string, status: string}> members this device may assign */
    #[Locked]
    public array $members = [];

    /** @var array<string, string> schedule key => chosen member id */
    public array $assign = [];

    public function mount(): void
    {
        $this->refetch();
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get('schedules'));
        if ($answer === null) {
            return;
        }
        $this->records = [];
        foreach ((array) ($answer['schedules'] ?? []) as $schedule) {
            if (is_array($schedule) && is_string($schedule['schedule_key'] ?? null)) {
                $this->hold('schedule', $schedule['schedule_key'], $schedule);
            }
        }

        $this->members = [];
        if ($this->mayAssign()) {
            $list = $this->attempt(fn () => $this->director()->get('members')) ?? [];
            foreach ((array) ($list['members'] ?? []) as $member) {
                if (is_array($member) && is_string($member['id'] ?? null)) {
                    $this->members[] = ['id' => $member['id'], 'display_name' => (string) ($member['display_name'] ?? $member['id']),
                        'role' => (string) ($member['role'] ?? ''), 'status' => (string) ($member['status'] ?? '')];
                }
            }
        }
    }

    protected function follows(array $change): bool
    {
        return ($change['kind'] ?? null) === 'schedule';
    }

    /**
     * Owners and admins reassign schedules: they hold manage_schedules and list members to choose from.
     */
    public function mayAssign(): bool
    {
        $identity = app(Identity::class);

        return is_array($identity->me()['capabilities'] ?? null) && $identity->can('manage_schedules') && $identity->can('manage_members');
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function schedules(): array
    {
        $schedules = array_values(array_filter($this->heldOf('schedule'), fn (array $schedule): bool => ! filled($schedule['removed_at'] ?? null)));
        usort($schedules, fn (array $a, array $b): int => strcmp((string) $a['schedule_key'], (string) $b['schedule_key']));

        return $schedules;
    }

    /**
     * Why a schedule's new firings are paused: the Director's `paused_reason` when it sends one, else a guess from its
     * responsible member being disabled (known only when this device lists members). Null when not paused.
     *
     * @param  array<string, mixed>  $schedule
     */
    public function paused(array $schedule): ?string
    {
        $words = PausedReason::words($schedule);
        if ($words !== null) {
            return $words !== '' ? $words : null;
        }

        foreach ($this->members as $member) {
            if ($member['id'] === ($schedule['responsible_member_id'] ?? null)) {
                return $member['status'] !== 'active' ? __('Paused: its member is disabled') : null;
            }
        }

        return null;
    }

    public function reassign(string $key): void
    {
        $schedule = $this->held('schedule', $key);
        $memberId = $this->assign[$key] ?? '';
        $active = [...array_column(array_filter($this->members, fn (array $member): bool => $member['status'] === 'active'), 'id'), app(Identity::class)->memberId()];
        if ($schedule === null || ! in_array($memberId, $active, true) || ! $this->mayAssign()) {
            return;
        }

        $answer = $this->attempt(fn () => $this->director()->put(
            'schedules/'.rawurlencode((string) $schedule['node']).'/'.rawurlencode((string) $schedule['schedule_id']),
            ['responsible_member_id' => $memberId],
            DirectorClient::newKey(),
            'PATCH',
        ));
        if (is_array($answer['schedule'] ?? null)) {
            $this->hold('schedule', $key, $answer['schedule']);
            unset($this->assign[$key]);
            Flux::toast(variant: 'success', text: __(':name is now responsible for it.', ['name' => (string) app(MemberNames::class)->label($memberId)]));
        }
    }
}; ?>

<section class="mx-auto flex w-full max-w-5xl flex-col gap-5" data-test="schedules-page" x-data="directorStream(@js(route('stream')))">
    <header class="flex items-start justify-between">
        <div>
            <flux:heading size="lg" level="1" class="text-lg!">{{ __('Schedules') }}</flux:heading>
            <flux:text class="mt-0.5">{{ __('Each machine fires its own schedules. A schedule runs as the member responsible for it, and pauses while that member cannot run work.') }}</flux:text>
        </div>
        <x-client.stream-dot />
    </header>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @forelse ($this->schedules() as $schedule)
        @php($key = (string) $schedule['schedule_key'])
        <div class="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-zinc-200 px-4 py-3 dark:border-zinc-700" wire:key="schedule-{{ $key }}" data-test="schedule-row">
            <div class="min-w-0 text-sm">
                <div class="font-mono">{{ $key }}</div>
                <div class="text-zinc-500">
                    {{ $schedule['cron'] ?? '' }} {{ $schedule['timezone'] ?? '' }} · {{ ($schedule['enabled'] ?? false) ? __('on') : __('off') }}
                    @if (($who = app(MemberNames::class)->label($schedule['responsible_member_id'] ?? null, $schedule['responsible_display_name'] ?? null)) !== null)
                        · {{ __('responsible') }}: <span data-test="schedule-responsible">{{ $who }}</span>
                    @endif
                </div>
                @if (($paused = $this->paused($schedule)) !== null)
                    <flux:badge size="sm" color="amber" data-test="schedule-paused">{{ $paused }}</flux:badge>
                @endif
            </div>
            @if ($this->mayAssign())
                <form wire:submit="reassign({{ Js::from($key) }})" class="flex items-end gap-2" data-test="schedule-reassign">
                    <flux:select size="sm" wire:model="assign.{{ $key }}" :placeholder="__('Hand to…')" :aria-label="__('Responsible member')" class="w-44">
                        @foreach ($members as $member)
                            @if ($member['status'] === 'active')
                                <flux:select.option :value="$member['id']">{{ $member['display_name'] }}</flux:select.option>
                            @endif
                        @endforeach
                        @if (! collect($members)->contains('id', app(Identity::class)->memberId()) && app(Identity::class)->memberId() !== null)
                            <flux:select.option :value="app(Identity::class)->memberId()">{{ __('Me') }}</flux:select.option>
                        @endif
                    </flux:select>
                    <flux:button size="sm" type="submit">{{ __('Reassign') }}</flux:button>
                </form>
            @endif
        </div>
    @empty
        <flux:text>{{ __('No schedules yet.') }}</flux:text>
    @endforelse
</section>
