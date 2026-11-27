<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\DirectorScreen;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

/**
 * One task lane (CLIENT-API.md: jobs, intents, runs and decisions): its state, the model it runs on and why (the
 * router's `meta.route` when it chose), where it works, the check's verdict, the other jobs of the lane, and the
 * phone's actions: approve or reject the finished run, cancel a job that is still going. Landing stays on the machine
 * (`cs land`), so the screen shows the command. Each action keeps one Idempotency-Key until it is recorded.
 */
class Lane extends DirectorScreen
{
    public string $jobId = '';

    /** @var array<string, mixed> */
    public array $job = [];

    /** @var array<string, mixed> */
    public array $intent = [];

    /** @var array<string, mixed> the job's latest run, or empty */
    public array $run = [];

    /** @var array<string, mixed> the run's decision receipt, or empty */
    public array $receipt = [];

    /** @var list<array{job_id: string, state: string, phase: string}> every job of the lane, oldest first */
    public array $attempts = [];

    public string $cancelKey = '';

    public string $decisionKey = '';

    public bool $loaded = false;

    public function mount(): void
    {
        $this->jobId = Json::string($this->param('id'));
        $this->cancelKey = DirectorClient::newKey();
        $this->decisionKey = DirectorClient::newKey();
        $this->refresh();
    }

    public function onResume(): void
    {
        $this->refresh();
    }

    /**
     * There is no event stream on the phone yet, so the open lane checks again every 10 seconds.
     */
    #[Poll(10000)]
    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $answer = $director->get('jobs/'.rawurlencode($this->jobId));
            $this->job = Json::record($answer['job'] ?? $answer);

            $intentId = Json::string($this->job['intent_id'] ?? null);
            if ($intentId !== '') {
                $bundle = $director->get('intents/'.rawurlencode($intentId));
                $this->intent = Json::record($bundle['intent'] ?? null);
                $jobs = Json::records($bundle['jobs'] ?? null);
                usort($jobs, fn (array $a, array $b): int => strcmp(Json::string($a['created_at'] ?? null), Json::string($b['created_at'] ?? null)));
                $this->attempts = array_map(fn (array $job): array => [
                    'job_id' => Json::string($job['job_id'] ?? null),
                    'state' => Json::string($job['state'] ?? null),
                    'phase' => Json::string(Json::record($job['meta'] ?? null)['phase'] ?? null),
                ], $jobs);
                $this->run = $this->latestRun(Json::records($bundle['runs'] ?? null));
            }

            $runId = Json::string($this->run['run_id'] ?? null);
            $this->receipt = $runId !== '' && is_array($this->run['decision'] ?? null)
                ? Json::record($director->find('runs/'.rawurlencode($runId).'/decision'))
                : [];
            $this->loaded = true;
        });
    }

    public function approveRun(): void
    {
        $this->decide('approve');
    }

    public function rejectRun(): void
    {
        $this->decide('reject');
    }

    public function cancelJob(): void
    {
        try {
            $this->director()->write('jobs/'.rawurlencode($this->jobId).'/cancel', [], $this->cancelKey);
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the cancel may not have arrived. Asking again is safe.';

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->cancelKey = DirectorClient::newKey();
            }
            $this->refresh();
            $this->problem = $refused->is('job_finished') ? 'The job is already over; there is nothing to cancel.' : $refused->explain();

            return;
        }

        $this->cancelKey = DirectorClient::newKey();
        $this->problem = '';
        $this->refresh();
    }

    public function title(): string
    {
        return Json::string($this->intent['title'] ?? null, Json::string($this->intent['task'] ?? null, 'Task lane'));
    }

    /**
     * @return array{model: string, reason: string}
     */
    public function modelRoute(): array
    {
        $route = Json::record(Json::record($this->job['meta'] ?? null)['route'] ?? null);
        $model = Json::string($route['model'] ?? null);

        return [
            'model' => $model !== '' ? $model : Json::string(Json::record($this->intent['meta'] ?? null)['model'] ?? null),
            'reason' => Json::string($route['reason'] ?? null),
        ];
    }

    public function where(): string
    {
        $meta = Json::record($this->intent['meta'] ?? null);
        $repo = Json::string($meta['repo'] ?? null);

        return $repo !== '' ? $repo.'/'.Json::string($meta['lane'] ?? null) : 'scratch workspace';
    }

    /**
     * The planning job of an Auto or plan-first lane: it writes a plan and changes no code, so it is never landed.
     */
    public function planning(): bool
    {
        return Json::string(Json::record($this->job['meta'] ?? null)['phase'] ?? null) === 'planning';
    }

    /**
     * Cancel is offered while the job is going, and again once a cancel is applied but the job has not ended: the
     * second request finishes a job that never reports (CLIENT-API.md §6.2).
     */
    public function canCancel(): bool
    {
        $state = $this->cancelState();

        return ! $this->finished() && ($state === '' || $state === 'applied')
            && (self::action($this->job, 'cancel') ?? ($this->can('control_work') && $this->mine($this->intent) && $this->mine($this->job)));
    }

    public function finished(): bool
    {
        return in_array($this->job['state'] ?? null, ['done', 'error'], true);
    }

    public function cancelState(): string
    {
        return Json::string(Json::record($this->job['cancel'] ?? null)['state'] ?? null);
    }

    public function canDecide(): bool
    {
        return $this->run !== [] && $this->receipt === [] && ! $this->landed()
            && (self::action($this->run, 'decide') ?? ($this->can('decide_run') && $this->mine($this->intent) && $this->mine($this->job)));
    }

    public function landed(): bool
    {
        return Json::string(Json::record($this->job['land'] ?? null)['state'] ?? null) === 'landed';
    }

    public function landCommand(): string
    {
        $meta = Json::record($this->intent['meta'] ?? null);
        $repo = Json::string($meta['repo'] ?? null);
        $lane = Json::string($meta['lane'] ?? null);
        $decision = $this->receipt['decision'] ?? null;
        $approved = $decision === 'approve' || ($decision === null && ($this->run['verdict'] ?? null) === 'pass');
        if ($this->planning()) {
            return '';
        }

        if ($this->landed() || ! $approved || ($this->job['state'] ?? null) !== 'done'
            || ! preg_match('/\A[a-z0-9._-]+\z/', $repo) || ! preg_match('/\A[a-z0-9._-]+\z/', $lane)) {
            return '';
        }

        return 'cs land '.$repo.' '.$lane;
    }

    public function render(): View
    {
        return view('native.lane');
    }

    private function decide(string $decision): void
    {
        $runId = Json::string($this->run['run_id'] ?? null);
        if ($runId === '' || ! $this->canDecide()) {
            return;
        }

        try {
            $this->director()->write('runs/'.rawurlencode($runId).'/decision', ['decision' => $decision], $this->decisionKey);
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the decision may not have arrived. Deciding again is safe.';

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->decisionKey = DirectorClient::newKey();
            }
            $this->refresh();
            $this->problem = $refused->is('already_decided') ? 'Someone has already decided this run.' : $refused->explain();

            return;
        }

        $this->decisionKey = DirectorClient::newKey();
        $this->problem = '';
        $this->refresh();
    }

    /**
     * The job's own run, or the newest of its runs.
     *
     * @param  list<array<string, mixed>>  $runs
     * @return array<string, mixed>
     */
    private function latestRun(array $runs): array
    {
        $byId = [];
        foreach ($runs as $run) {
            $byId[Json::string($run['run_id'] ?? null)] = $run;
        }

        $runId = Json::string($this->job['run_id'] ?? null);
        if ($runId === '') {
            $runIds = Json::strings($this->job['run_ids'] ?? null);
            $runId = $runIds !== [] ? $runIds[array_key_last($runIds)] : '';
        }

        return $byId[$runId] ?? [];
    }
}
