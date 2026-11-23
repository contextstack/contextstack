<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Json;
use App\Native\DirectorScreen;
use Illuminate\View\View;

/**
 * The Director's projects; each opens its backlog.
 */
class Projects extends DirectorScreen
{
    /** @var list<array{id: string, name: string}> */
    public array $projects = [];

    public bool $loaded = false;

    public function mount(): void
    {
        $this->refresh();
    }

    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $this->projects = [];
            foreach (Json::records($director->get('projects')['projects'] ?? null) as $project) {
                $id = Json::string($project['project_id'] ?? null);
                if ($id !== '') {
                    $this->projects[] = ['id' => $id, 'name' => Json::string($project['name'] ?? null, $id)];
                }
            }
            usort($this->projects, fn (array $a, array $b): int => strcasecmp($a['name'], $b['name']));
            $this->loaded = true;
        });
    }

    public function render(): View
    {
        return view('native.projects');
    }
}
