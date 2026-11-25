<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\DirectorScreen;
use App\Native\ModelPicker;
use Illuminate\Support\Str;
use Illuminate\View\View;

/**
 * Start a task lane on a repo (CLIENT-API.md: POST intents with a `lane`). The model defaults to Auto: the Director
 * plans first and picks the coding model from the plan, recording why on the coding job. An explicit model from the
 * catalog can plan first too (`lane.plan_with`). Scratch lanes stay on the desktop. The request keeps one
 * Idempotency-Key until the Director has recorded it, so starting again after a lost answer does not start twice.
 */
class NewLane extends DirectorScreen
{
    /** The first provider choice: no model, the Director plans first and picks one. */
    public const Auto = 'Auto: plan first, then pick the model';

    public const NoPlan = 'No';

    /** The planners a person can choose for an explicit model, by the words the picker shows. */
    public const Planners = ['Codex' => 'codex', 'Claude Sonnet' => 'claude-code-sonnet'];

    public string $title = '';

    public string $goal = '';

    public string $repo = '';

    public string $lane = '';

    public bool $laneEdited = false;

    public string $provider = self::Auto;

    public string $model = '';

    /** The harness that runs the model, when more than one can ('' when the catalog names none). */
    public string $harness = '';

    public string $planFirst = self::NoPlan;

    public string $node = '';

    /** @var list<string> each repo name once */
    public array $repos = [];

    /** @var array<string, array<string, bool>> repo => (machine reporting it => whether it is current, not stale) */
    public array $repoNodes = [];

    /** @var array<string, mixed> GET models, kept so the picker can be rebuilt */
    public array $catalog = [];

    public string $submitKey = '';

    public bool $loaded = false;

    public function mount(): void
    {
        $this->submitKey = DirectorClient::newKey();
        $this->attempt(function (DirectorClient $director): void {
            $live = [];
            foreach (Json::records($director->get('nodes')['nodes'] ?? null) as $node) {
                $name = Json::string($node['name'] ?? null);
                if ($name !== '' && ($node['revoked_at'] ?? null) === null) {
                    $live[$name] = ($node['stale'] ?? false) !== true;
                }
            }
            foreach (Json::records($director->get('repos')['repos'] ?? null) as $repo) {
                $name = Json::string($repo['name'] ?? null);
                $node = Json::string($repo['node'] ?? null);
                if ($name !== '' && isset($live[$node])) {
                    $this->repoNodes[$name][$node] = $live[$node];
                }
            }
            $this->repos = array_keys($this->repoNodes);
            sort($this->repos);
            $this->repo = $this->repos[0] ?? '';
            $this->updatedRepo();
            $this->catalog = $director->get('models');
            $this->loaded = true;
        });
    }

    /**
     * Run the lane on a machine that has the repo, preferring one that is current over a stale one.
     */
    public function updatedRepo(): void
    {
        $nodes = $this->repoNodes[$this->repo] ?? [];
        arsort($nodes);
        $this->node = (string) array_key_first($nodes);
    }

    /**
     * @return list<string>
     */
    public function nodeChoices(): array
    {
        return array_keys($this->repoNodes[$this->repo] ?? []);
    }

    public function updatedTitle(): void
    {
        if (! $this->laneEdited) {
            $this->lane = Str::limit(Str::slug($this->title), 63, '');
        }
    }

    /**
     * The person named the lane themselves, so the title stops renaming it.
     */
    public function updatedLane(): void
    {
        $this->laneEdited = $this->lane !== '';
    }

    public function updatedProvider(): void
    {
        $this->model = $this->isAuto() ? '' : $this->picker()->firstModel($this->provider);
        $this->updatedModel();
    }

    public function updatedModel(): void
    {
        $this->harness = $this->isAuto() ? '' : $this->picker()->firstHarness($this->provider, $this->model);
    }

    /**
     * @return list<string>
     */
    public function harnessChoices(): array
    {
        return $this->picker()->harnessChoices($this->provider, $this->model);
    }

    public function picker(): ModelPicker
    {
        return ModelPicker::fromCatalog($this->catalog);
    }

    /**
     * @return list<string>
     */
    public function providerChoices(): array
    {
        return [self::Auto, ...$this->picker()->providerChoices()];
    }

    /**
     * @return list<string>
     */
    public function modelChoices(): array
    {
        return $this->picker()->modelChoices($this->provider);
    }

    public function where(): string
    {
        return $this->isAuto() ? '' : $this->picker()->where($this->picker()->value($this->provider, $this->model, $this->harness));
    }

    /**
     * @return list<string>
     */
    public function planChoices(): array
    {
        return [self::NoPlan, ...array_keys(self::Planners)];
    }

    public function isAuto(): bool
    {
        return $this->provider === self::Auto;
    }

    public function start(): void
    {
        $title = trim($this->title);
        $goal = trim($this->goal);
        $model = $this->isAuto() ? 'auto' : $this->picker()->value($this->provider, $this->model, $this->harness);

        $missing = match (true) {
            $title === '' => 'Give the lane a title.',
            $goal === '' => 'Say what the lane should do.',
            $this->repo === '' || ! in_array($this->repo, $this->repos, true) => 'Choose a repo.',
            ! preg_match('/\A[a-z0-9][a-z0-9._-]{0,62}\z/', $this->lane) => 'Name the lane with lowercase letters, numbers, dots, dashes or underscores.',
            $model === '' => 'Choose a model.',
            $this->node === '' || ! isset($this->repoNodes[$this->repo][$this->node]) => 'No machine reports this repo, so the lane has nowhere to run.',
            default => '',
        };
        if ($missing !== '') {
            $this->problem = $missing;

            return;
        }

        $lane = ['title' => $title, 'goal' => $goal, 'model' => $model, 'repo' => $this->repo, 'lane' => $this->lane];
        $planner = self::Planners[$this->planFirst] ?? null;
        if (! $this->isAuto() && $planner !== null) {
            $lane['plan_with'] = $planner;
        }

        try {
            $answer = $this->director()->write('intents', ['node' => $this->node, 'lane' => $lane, 'meta' => ['source' => 'client-mobile-lane']], $this->submitKey);
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the lane may not have started. Starting again is safe.';

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->submitKey = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return;
        }

        $this->submitKey = DirectorClient::newKey();
        $jobs = Json::records($answer->body['jobs'] ?? null);
        $jobId = Json::string(($jobs[array_key_last($jobs)] ?? [])['job_id'] ?? null);
        $this->replace($jobId !== '' ? '/lanes/'.rawurlencode($jobId) : '/lanes');
    }

    public function render(): View
    {
        return view('native.new-lane');
    }
}
