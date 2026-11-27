<?php

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Livewire\DirectorPage;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Source budgets (CLIENT-API.md §12.5): the gateway's budget per pay-as-you-go source, read through the Director. A
 * source with no budget is off ("off, set a budget to use"), which is not the same as "budget used up". Only an owner
 * (the Director's machine or a paired device) may set or turn one off; any other credential gets 403 owner_only, and
 * this page shows that refusal as it is. Budget writes replace what they name, so they carry no key.
 */
new #[Layout('layouts::client')] #[Title('Source budgets')] class extends DirectorPage
{
    public string $source = '';

    public string $usd = '';

    public string $period = 'day';

    #[Locked]
    public ?bool $owner = null;

    /** @var array<string, mixed> */
    #[Locked]
    public array $report = [];

    /** @var array{status: int, code: string, detail: string}|array{} */
    #[Locked]
    public array $refusal = [];

    public function mount(): void
    {
        $me = $this->attempt(fn () => $this->director()->get('me'));
        $this->owner = is_array($me) ? (bool) ($me['owner'] ?? false) : null;
        $this->refetch();
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get('source-budgets'));
        if (is_array($answer)) {
            $this->report = $answer;
            $this->source = $this->source !== '' ? $this->source : (string) ($this->sources()[0]['source'] ?? '*');
        }
    }

    protected function follows(array $change): bool
    {
        return false;
    }

    public function setSource(): void
    {
        $this->validate([
            'source' => ['required', 'string', 'max:128', 'regex:/^(\*|[a-z0-9][a-z0-9._-]*)$/'],
            'usd' => ['required', 'numeric', 'min:0', 'max:1000000'],
            'period' => ['required', 'in:day,month'],
        ]);

        $this->write($this->source, ['usd' => $this->usd, 'period' => $this->period]);
    }

    public function turnOff(string $source): void
    {
        abort_unless(preg_match('/^(\*|[a-z0-9][a-z0-9._-]*)$/', $source) === 1, 422);

        $this->write($source, null);
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function sources(): array
    {
        return array_values(array_filter((array) ($this->report['budgets']['sources'] ?? []), 'is_array'));
    }

    /**
     * @param  array<string, mixed>|null  $body
     */
    private function write(string $source, ?array $body): void
    {
        $this->refusal = [];
        try {
            $answer = $this->director()->replace('source-budgets/'.rawurlencode($source), $body);
        } catch (DirectorRefused $refused) {
            $this->refusal = ['status' => $refused->status, 'code' => $refused->errorCode, 'detail' => $refused->explain()];

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Setting it again is safe: it replaces the entry.', ['why' => $unavailable->getMessage()]);

            return;
        }

        $this->problem = '';
        $this->report = $answer;
    }
};
?>

<section class="mx-auto flex w-full max-w-5xl flex-col gap-5" data-test="sources-page">
    <header>
        <flux:heading size="lg" level="1" class="text-lg!">{{ __('Source budgets') }}</flux:heading>
        <flux:text class="mt-0.5 max-w-3xl">{{ __('The gateway holds a budget per pay-as-you-go source; a source without one is off. Subscriptions and local models have no dollars to budget. Project and conversation budgets narrow these further.') }}</flux:text>
    </header>

    @if ($owner === false)
        <flux:callout icon="lock-closed" data-test="device-note">
            <flux:callout.text>{{ __('This credential is not the owner\'s. It can read source budgets; the Director refuses it setting one (403 owner_only).') }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($refusal !== [])
        <flux:callout variant="danger" icon="no-symbol" data-test="refusal" data-code="{{ $refusal['code'] }}">
            <flux:callout.heading>{{ __('The Director refused: :status :code', ['status' => $refusal['status'], 'code' => $refusal['code']]) }}</flux:callout.heading>
            <flux:callout.text>{{ $refusal['detail'] }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if ($report !== [])
        @php($total = $report['budgets']['total'] ?? null)
        <flux:card class="space-y-1" data-test="shared-budget">
            <flux:heading>{{ __('Shared budget (*)') }}</flux:heading>
            @if (is_array($total))
                <flux:text>{{ __(':usd per :period, :left left; it covers every source without its own entry.', ['usd' => \App\Client\Money::dollars($total['usd']), 'period' => $total['period'] ?? 'day', 'left' => \App\Client\Money::dollars($total['remaining_usd'] ?? 0)]) }}</flux:text>
                <flux:button size="xs" variant="ghost" wire:click="turnOff('*')">{{ __('Turn off the shared budget') }}</flux:button>
            @else
                <flux:text>{{ __('None.') }}</flux:text>
            @endif
        </flux:card>

        <flux:table>
            <flux:table.columns>
                <flux:table.column>{{ __('Source') }}</flux:table.column>
                <flux:table.column>{{ __('State') }}</flux:table.column>
                <flux:table.column>{{ __('Budget') }}</flux:table.column>
                <flux:table.column>{{ __('Charged today / this month') }}</flux:table.column>
                <flux:table.column></flux:table.column>
            </flux:table.columns>
            <flux:table.rows>
                @foreach ($this->sources() as $row)
                    @php($state = (string) ($row['state'] ?? 'off'))
                    <flux:table.row wire:key="source-{{ $row['source'] }}" data-test="source-row" data-source="{{ $row['source'] }}" data-state="{{ $state }}">
                        <flux:table.cell class="font-mono">{{ $row['source'] }}</flux:table.cell>
                        <flux:table.cell>
                            @php($stateLabel = match ($state) { 'active' => __('active'), 'exhausted' => __('budget used up'), default => __('off') })
                            {{-- The gateway's line starts with the state the badge already shows ("off, set a budget to use"). --}}
                            @php($detail = preg_replace('/^'.preg_quote($stateLabel, '/').'[,:]?\s*/iu', '', (string) ($row['shown'] ?? '')))
                            <flux:badge size="sm" :color="match ($state) { 'active' => 'green', 'exhausted' => 'red', default => 'zinc' }">{{ $stateLabel }}</flux:badge>
                            <flux:text class="text-sm" data-test="source-shown">{{ $detail }}</flux:text>
                        </flux:table.cell>
                        <flux:table.cell>
                            @if (is_array($row['budget'] ?? null))
                                {{ \App\Client\Money::dollars($row['budget']['usd']) }} / {{ $row['budget']['period'] ?? 'day' }}
                            @elseif ($row['shared'] ?? false)
                                {{ __('the shared budget') }}
                            @else
                                —
                            @endif
                        </flux:table.cell>
                        <flux:table.cell>{{ \App\Client\Money::dollars($row['spent']['day_usd'] ?? 0) }} / {{ \App\Client\Money::dollars($row['spent']['month_usd'] ?? 0) }}</flux:table.cell>
                        <flux:table.cell>
                            @if ($state !== 'off')
                                @capable('manage_source_budgets')
                                <flux:button size="xs" variant="ghost" wire:click="turnOff({{ Js::from($row['source']) }})" data-test="turn-off">{{ __('Turn off') }}</flux:button>
                                @endcapable
                            @endif
                        </flux:table.cell>
                    </flux:table.row>
                @endforeach
            </flux:table.rows>
        </flux:table>

        @capable('manage_source_budgets')
        <flux:card>
            <form wire:submit="setSource" class="flex flex-wrap items-end gap-3" data-test="set-source">
                <flux:select wire:model="source" :label="__('Source')" class="w-48">
                    <flux:select.option value="*">{{ __('* (shared)') }}</flux:select.option>
                    @foreach ($this->sources() as $row)
                        <flux:select.option :value="$row['source']" wire:key="option-{{ $row['source'] }}">{{ $row['source'] }}</flux:select.option>
                    @endforeach
                </flux:select>
                <flux:input wire:model="usd" :label="__('Dollars')" type="number" step="0.01" min="0" class="w-32" />
                <flux:select wire:model="period" :label="__('Per')" class="w-32">
                    <flux:select.option value="day">{{ __('day') }}</flux:select.option>
                    <flux:select.option value="month">{{ __('month') }}</flux:select.option>
                </flux:select>
                <flux:button type="submit" variant="primary">{{ __('Set source budget') }}</flux:button>
            </form>
        </flux:card>
        @endcapable
    @endif
</section>
