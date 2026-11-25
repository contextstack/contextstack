<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\DirectorScreen;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * Source budgets (CLIENT-API.md §12.5): the gateway's dollar budget per pay-as-you-go source, and the shared budget
 * (`*`) that covers every source without its own. A source with no budget is off. Setting or turning off a budget
 * changes what the factory may spend, so it first asks for this device's own unlock, as pairing does. A budget
 * write replaces the entry it names, so sending it again is safe and it carries no key.
 */
class Budgets extends DirectorScreen
{
    public const Shared = '*';

    /** The periods a budget can cover, by the words the picker shows. */
    public const Periods = ['Per day' => 'day', 'Per month' => 'month'];

    /** @var list<array{source: string, state: string, shown: string, budget: string, spent: string}> */
    public array $sources = [];

    public string $shared = '';

    /** The source being edited, or '' when none is. */
    public string $editing = '';

    public string $usd = '';

    public string $period = 'Per day';

    public bool $loaded = false;

    public function mount(): void
    {
        $this->refresh();
    }

    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $this->show($director->get('source-budgets'));
        });
    }

    public function edit(string $source): void
    {
        $this->editing = $source;
        $this->usd = '';
        $this->problem = '';
    }

    public function closeEdit(): void
    {
        $this->editing = '';
    }

    /**
     * @return list<string>
     */
    public function periodChoices(): array
    {
        return array_keys(self::Periods);
    }

    public function setBudget(): void
    {
        if (! is_numeric($this->usd) || (float) $this->usd < 0 || (float) $this->usd > 1000000) {
            $this->problem = 'Enter a dollar amount, such as 5 or 0.50.';

            return;
        }
        if (! $this->validSource($this->editing)) {
            return;
        }

        Biometrics::prompt()->id('budget-set')->prompt();
    }

    public function turnOff(string $source): void
    {
        if (! $this->validSource($source)) {
            return;
        }
        $this->editing = $source;

        Biometrics::prompt()->id('budget-off')->prompt();
    }

    /**
     * The device unlock answered. Only a success for a budget change goes on to the Director.
     */
    #[On(Completed::class)]
    public function unlocked(bool $success, ?string $id = null): void
    {
        if ($id !== 'budget-set' && $id !== 'budget-off') {
            return;
        }
        if (! $success) {
            $this->problem = 'Not unlocked, so the budget did not change.';

            return;
        }

        $body = $id === 'budget-set' ? ['usd' => (float) $this->usd, 'period' => self::Periods[$this->period] ?? 'day'] : null;

        try {
            $this->show($this->director()->replace('source-budgets/'.rawurlencode($this->editing), $body));
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the budget may not have changed. Doing it again is safe.';

            return;
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();

            return;
        }

        $this->problem = '';
        $this->editing = '';
    }

    public function render(): View
    {
        return view('native.budgets');
    }

    /**
     * @param  array<string, mixed>  $report
     */
    private function show(array $report): void
    {
        $budgets = Json::record($report['budgets'] ?? null);
        $total = Json::record($budgets['total'] ?? null);
        $this->shared = $total !== []
            ? self::dollars($total['usd'] ?? 0).' per '.Json::string($total['period'] ?? null, 'day').', '.self::dollars($total['remaining_usd'] ?? 0).' left'
            : '';

        $this->sources = [];
        foreach (Json::records($budgets['sources'] ?? null) as $row) {
            $source = Json::string($row['source'] ?? null);
            if ($source === '') {
                continue;
            }
            $budget = Json::record($row['budget'] ?? null);
            $spent = Json::record($row['spent'] ?? null);
            $this->sources[] = [
                'source' => $source,
                'state' => Json::string($row['state'] ?? null, 'off'),
                'shown' => Json::string($row['shown'] ?? null),
                'budget' => $budget !== []
                    ? self::dollars($budget['usd'] ?? 0).' per '.Json::string($budget['period'] ?? null, 'day')
                    : (($row['shared'] ?? false) === true ? 'the shared budget' : 'no budget'),
                'spent' => self::dollars($spent['day_usd'] ?? 0).' today, '.self::dollars($spent['month_usd'] ?? 0).' this month',
            ];
        }
        $this->loaded = true;
    }

    private function validSource(string $source): bool
    {
        if ($source === self::Shared || in_array($source, array_column($this->sources, 'source'), true)) {
            return true;
        }
        $this->problem = 'Choose a source first.';

        return false;
    }

    private static function dollars(mixed $usd): string
    {
        return '$'.number_format(is_int($usd) || is_float($usd) ? (float) $usd : 0.0, 2);
    }
}
