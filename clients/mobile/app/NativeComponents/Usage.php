<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Json;
use App\Native\DirectorScreen;
use Illuminate\View\View;

/**
 * What the runs used (CLIENT-API.md §12.4: GET usage), by source, for today, this month or all time. Each source is
 * shown for what it is: pay as you go with the dollars actually charged, a subscription with the allowance its agent
 * reported, local with usage only. An API-price estimate is not a charge, so it is never shown.
 */
class Usage extends DirectorScreen
{
    /** The periods a person can choose, by the words the picker shows. */
    public const Periods = ['Today' => 'day', 'This month' => 'month', 'All time' => 'all'];

    public string $period = 'Today';

    /** @var list<array{model: string, kind: string, runs: int, figures: string}> */
    public array $sources = [];

    /** @var list<string> one line per kind of source, for the total */
    public array $total = [];

    public string $range = '';

    public bool $loaded = false;

    public function mount(): void
    {
        $this->refresh();
    }

    public function onResume(): void
    {
        $this->refresh();
    }

    public function updatedPeriod(): void
    {
        $this->refresh();
    }

    public function refresh(): void
    {
        $period = self::Periods[$this->period] ?? 'day';
        $this->attempt(function (DirectorClient $director) use ($period): void {
            $answer = $director->get('usage', ['by' => 'source', 'period' => $period]);
            $this->sources = [];
            foreach (Json::records($answer['groups'] ?? null) as $group) {
                $runs = $group['runs'] ?? 0;
                $kind = Json::string($group['kind'] ?? null, 'unknown');
                $this->sources[] = [
                    'model' => Json::string(Json::record($group['source'] ?? null)['model'] ?? null, '(no model)'),
                    'kind' => self::kindWords($kind),
                    'runs' => is_int($runs) ? $runs : 0,
                    'figures' => self::figures($kind, $group),
                ];
            }
            $this->total = [];
            foreach (Json::record(Json::record($answer['total'] ?? null)['kinds'] ?? null) as $kind => $figures) {
                $figures = Json::record($figures);
                $runs = $figures['runs'] ?? 0;
                if (is_int($runs) && $runs > 0) {
                    $this->total[] = ucfirst(self::kindWords((string) $kind)).': '.self::runs($runs).self::tail(self::figures((string) $kind, $figures));
                }
            }
            $from = Json::string($answer['from'] ?? null);
            $this->range = $from !== '' ? $from.' to '.Json::string($answer['to'] ?? null).' (UTC)' : '';
            $this->loaded = true;
        });
    }

    /**
     * @return list<string>
     */
    public function periodChoices(): array
    {
        return array_keys(self::Periods);
    }

    /**
     * @param  array{model: string, kind: string, runs: int, figures: string}  $source
     */
    public static function summary(array $source): string
    {
        return $source['kind'].' · '.self::runs($source['runs']).self::tail($source['figures']);
    }

    public static function kindWords(string $kind): string
    {
        return match ($kind) {
            'pay_as_you_go' => 'pay as you go',
            'subscription' => 'subscription',
            'local' => 'local',
            default => 'unknown source',
        };
    }

    /**
     * What a source's figures say, for its kind: never an estimate.
     *
     * @param  array<string, mixed>  $figures
     */
    public static function figures(string $kind, array $figures): string
    {
        return match ($kind) {
            'pay_as_you_go' => self::dollars($figures['charged_usd'] ?? 0).' charged'
                .(is_int($figures['charge_unreported_runs'] ?? null) && $figures['charge_unreported_runs'] > 0
                    ? ', '.self::runs($figures['charge_unreported_runs']).' reported no charge' : ''),
            'subscription' => trim(Json::string($figures['shown'] ?? null).(($figures['allowance_reached'] ?? false) === true ? ' (allowance reached)' : '')),
            'local' => 'usage only, no cost',
            default => '',
        };
    }

    public function render(): View
    {
        return view('native.usage');
    }

    private static function dollars(mixed $usd): string
    {
        return '$'.number_format(is_int($usd) || is_float($usd) ? (float) $usd : 0.0, 2);
    }

    private static function runs(int $runs): string
    {
        return $runs === 1 ? '1 run' : $runs.' runs';
    }

    private static function tail(string $figures): string
    {
        return $figures !== '' ? ' · '.$figures : '';
    }
}
