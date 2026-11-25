<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Json;
use App\Native\DirectorScreen;
use App\Native\When;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

/**
 * The machines that pull work from the Director (CLIENT-API.md: GET nodes): whether each is listening, seen, stale
 * or revoked, when it was last heard from, its open jobs, and the models it is out of quota for.
 */
class Machines extends DirectorScreen
{
    /** @var list<array{name: string, state: string, seen: string, open_jobs: int, limits: list<string>}> */
    public array $machines = [];

    public int $staleAfter = 0;

    public bool $loaded = false;

    public function mount(): void
    {
        $this->refresh();
    }

    public function onResume(): void
    {
        $this->refresh();
    }

    /**
     * There is no event stream on the phone yet, so the open screen checks again every 30 seconds.
     */
    #[Poll(30000)]
    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $answer = $director->get('nodes');
            $staleAfter = $answer['stale_after_seconds'] ?? 0;
            $this->staleAfter = is_int($staleAfter) ? $staleAfter : 0;
            $this->machines = [];
            foreach (Json::records($answer['nodes'] ?? null) as $node) {
                $name = Json::string($node['name'] ?? null);
                if ($name === '') {
                    continue;
                }
                $openJobs = $node['open_jobs'] ?? 0;
                $this->machines[] = [
                    'name' => $name,
                    'state' => self::state($node),
                    'seen' => When::relative($node['last_seen_at'] ?? null, 'never'),
                    'open_jobs' => is_int($openJobs) ? $openJobs : 0,
                    'limits' => self::limits($node),
                ];
            }
            usort($this->machines, fn (array $a, array $b): int => strcmp($a['name'], $b['name']));
            $this->loaded = true;
        });
    }

    /**
     * @param  array{name: string, state: string, seen: string, open_jobs: int, limits: list<string>}  $machine
     */
    public static function summary(array $machine): string
    {
        $jobs = $machine['open_jobs'] === 1 ? '1 open job' : $machine['open_jobs'].' open jobs';

        return implode(' · ', array_filter([
            $machine['state'],
            'seen '.$machine['seen'],
            $jobs,
            $machine['limits'] !== [] ? 'out of quota: '.implode(', ', $machine['limits']) : '',
        ]));
    }

    public function render(): View
    {
        return view('native.machines');
    }

    /**
     * @param  array<string, mixed>  $node
     */
    private static function state(array $node): string
    {
        return match (true) {
            ($node['revoked_at'] ?? null) !== null => 'revoked',
            ($node['stale'] ?? true) !== false => 'stale',
            ($node['listening'] ?? false) === true => 'listening',
            default => 'seen',
        };
    }

    /**
     * The models this machine reported as out of quota whose reset time has not passed.
     *
     * @param  array<string, mixed>  $node
     * @return list<string>
     */
    private static function limits(array $node): array
    {
        $limited = [];
        foreach (Json::records(Json::record($node['status'] ?? null)['provider_limits'] ?? null) as $row) {
            $template = Json::string($row['template'] ?? null);
            $resets = Json::string($row['resets_at'] ?? null);
            if ($template === '' || ($resets !== '' && strtotime($resets) !== false && strtotime($resets) <= time())) {
                continue;
            }
            $limited[] = $template;
        }

        return $limited;
    }
}
