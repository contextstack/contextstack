<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\Backlog;
use App\Native\DirectorScreen;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

/**
 * One project's backlog: what waits on you, then the items in priority order (open ones, or all). Choosing an item
 * shows it beside the list on an iPad and in its place on a phone, with the quick changes a phone is for: start,
 * done, drop, and clearing the owner blocker once you have answered. Each change is sent against the revision shown,
 * so a stale one is refused (409) and the backlog is fetched again. Reordering and full editing stay on the desktop.
 */
class ProjectBacklog extends DirectorScreen
{
    public string $projectId = '';

    public string $projectName = '';

    /** @var array<string, array<string, mixed>> item id => item */
    public array $items = [];

    public bool $showClosed = false;

    public ?string $selected = null;

    public bool $loaded = false;

    public function mount(): void
    {
        $this->projectId = Json::string($this->param('id'));
        $this->selected = Json::nullableString($this->data('item'));
        $this->attempt(function (DirectorClient $director): void {
            $project = Json::record($director->get('projects/'.rawurlencode($this->projectId))['project'] ?? null);
            $this->projectName = Json::string($project['name'] ?? null, $this->projectId);
        });
        $this->refresh();
    }

    public function onResume(): void
    {
        $this->refresh();
    }

    /**
     * There is no event stream on the phone yet, so the open screen checks again every 20 seconds.
     */
    #[Poll(20000)]
    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $this->items = [];
            foreach (Backlog::ordered(Json::records($director->get($this->base())['items'] ?? null)) as $item) {
                $id = Json::string($item['item_id'] ?? null);
                if ($id !== '') {
                    $this->items[$id] = $item;
                }
            }
            $this->loaded = true;
        });

        if ($this->selected !== null && ! isset($this->items[$this->selected])) {
            $this->selected = null;
        }
    }

    public function toggleClosed(): void
    {
        $this->showClosed = ! $this->showClosed;
    }

    public function select(string $id): void
    {
        $this->selected = isset($this->items[$id]) ? $id : null;
    }

    public function closeDetail(): void
    {
        $this->selected = null;
    }

    public function setStatus(string $id, string $status): void
    {
        $item = $this->items[$id] ?? null;
        if ($item === null || ! in_array($status, ['next', 'now', 'blocked', 'done', 'dropped'], true)) {
            return;
        }
        $this->update($id, ['status' => $status, 'revision' => $item['revision'] ?? null]);
    }

    /**
     * The owner answered: remove the owner blockers, and a blocked item with nothing else blocking it goes back to next.
     */
    public function clearOwnerBlocker(string $id): void
    {
        $item = $this->items[$id] ?? null;
        if ($item === null) {
            return;
        }
        $remaining = array_values(array_filter(Json::records($item['blocked_on'] ?? null), fn (array $blocker): bool => ($blocker['kind'] ?? null) !== 'owner'));
        $changes = ['blocked_on' => $remaining, 'revision' => $item['revision'] ?? null];
        if ($remaining === [] && ($item['status'] ?? null) === 'blocked') {
            $changes['status'] = 'next';
        }
        $this->update($id, $changes);
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function shown(): array
    {
        return array_values(array_filter($this->items, fn (array $item): bool => $this->showClosed || in_array($item['status'] ?? 'next', Backlog::Open, true)));
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function waitingOnYou(): array
    {
        return array_values(array_filter($this->items, fn (array $item): bool => in_array($item['status'] ?? 'next', Backlog::Open, true) && Backlog::waitsOnOwner($item)));
    }

    public function render(): View
    {
        return view('native.project-backlog', [
            'item' => $this->selected !== null ? ($this->items[$this->selected] ?? null) : null,
        ]);
    }

    /**
     * @param  array<string, mixed>  $changes
     */
    private function update(string $id, array $changes): void
    {
        try {
            $answer = $this->director()->write($this->base().'/'.rawurlencode($id).'/update', $changes, $this->keyFor('update:'.$id.':'.md5((string) json_encode($changes))));
        } catch (DirectorRefused $refused) {
            $this->answered('update:'.$id.':'.md5((string) json_encode($changes)));
            if ($refused->status === 409) {
                $this->refresh();
                $this->problem = 'This item changed since you opened it. It now shows as it is; make your change again if it still applies.';

                return;
            }
            $this->problem = $refused->explain();

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the change may not have arrived. Trying again is safe.';

            return;
        }

        $this->answered('update:'.$id.':'.md5((string) json_encode($changes)));

        $this->problem = '';
        $updated = Json::record($answer->body['item'] ?? null);
        if ($updated !== []) {
            $this->items[$id] = $updated;
        }
    }

    private function base(): string
    {
        return 'projects/'.rawurlencode($this->projectId).'/backlog';
    }
}
