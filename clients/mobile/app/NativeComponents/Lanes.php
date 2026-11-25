<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Json;
use App\Native\DirectorScreen;
use App\Native\When;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

/**
 * Task lanes, newest first (CLIENT-API.md: GET intents?lanes=1, which the Director filters to lane intents): one row
 * per lane, showing its newest attempt. A lane
 * started with model auto has a planning job and then the coding job under one intent; the row follows the newest.
 * Each row opens the lane; the top bar starts a new one.
 */
class Lanes extends DirectorScreen
{
    public const Shown = 40;

    /** @var list<array{job_id: string, title: string, state: string, where: string, attempts: int, created_at: string}> */
    public array $lanes = [];

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
     * There is no event stream on the phone yet, so the open list checks again every 20 seconds.
     */
    #[Poll(20000)]
    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $answer = $director->get('intents', ['lanes' => '1', 'limit' => self::Shown]);
            $intents = [];
            foreach (Json::records($answer['intents'] ?? null) as $intent) {
                $id = Json::string($intent['intent_id'] ?? null);
                if ($id !== '') {
                    $intents[$id] = $intent;
                }
            }

            $byIntent = [];
            foreach (Json::records($answer['jobs'] ?? null) as $job) {
                $intentId = Json::string($job['intent_id'] ?? null);
                if (isset($intents[$intentId])) {
                    $byIntent[$intentId][] = $job;
                }
            }

            $this->lanes = [];
            foreach ($byIntent as $intentId => $jobs) {
                usort($jobs, fn (array $a, array $b): int => strcmp(Json::string($a['created_at'] ?? null), Json::string($b['created_at'] ?? null)));
                $newest = $jobs[array_key_last($jobs)];
                $meta = Json::record($intents[$intentId]['meta'] ?? null);
                $repo = Json::string($meta['repo'] ?? null);
                $this->lanes[] = [
                    'job_id' => Json::string($newest['job_id'] ?? null),
                    'title' => Json::string($intents[$intentId]['title'] ?? null, Json::string($intents[$intentId]['task'] ?? null, 'Task lane')),
                    'state' => Json::string($newest['state'] ?? null),
                    'where' => $repo !== '' ? $repo.'/'.Json::string($meta['lane'] ?? null) : 'scratch workspace',
                    'attempts' => count($jobs),
                    'created_at' => Json::string($newest['created_at'] ?? null),
                ];
            }
            usort($this->lanes, fn (array $a, array $b): int => strcmp($b['created_at'], $a['created_at']));
            $this->loaded = true;
        });
    }

    public function newLane(): void
    {
        $this->navigate('/lanes/new');
    }

    /**
     * @param  array{job_id: string, title: string, state: string, where: string, attempts: int, created_at: string}  $lane
     */
    public static function summary(array $lane): string
    {
        return implode(' · ', array_filter([
            $lane['state'],
            $lane['where'],
            $lane['attempts'] > 1 ? $lane['attempts'].' jobs' : '',
            When::relative($lane['created_at'], ''),
        ]));
    }

    public function render(): View
    {
        return view('native.lanes');
    }
}
