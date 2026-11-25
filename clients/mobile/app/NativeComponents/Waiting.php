<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\ActionApprovals;
use App\Native\DirectorScreen;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

/**
 * What waits on you: action approvals the reviewer passed to the owner, and conversations whose lead asked a
 * question. On a phone the list and the selected item take turns; from `md` width (an iPad) they sit side by side.
 */
class Waiting extends DirectorScreen
{
    /** @var array<string, array<string, mixed>> approval id => record, oldest first */
    public array $approvals = [];

    /** @var list<array{id: string, title: string, question: string}> */
    public array $questions = [];

    /** @var list<array{project_id: string, project: string, item_id: string, title: string, reason: string}> backlog items blocked on you */
    public array $backlog = [];

    public ?string $selected = null;

    /** Said when some project's backlog could not be read, while the rest of the list still shows. */
    public string $backlogProblem = '';

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
     * There is no event stream on the phone yet, so the open screen checks again every 15 seconds.
     */
    #[Poll(15000)]
    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $approvals = Json::records($director->get('action-approvals', ['status' => 'needs_owner'])['action_approvals'] ?? null);
            usort($approvals, fn (array $a, array $b): int => strcmp(Json::string($a['created_at'] ?? null), Json::string($b['created_at'] ?? null)));
            $this->approvals = [];
            foreach ($approvals as $approval) {
                $id = Json::string($approval['id'] ?? null);
                if ($id !== '') {
                    $this->approvals[$id] = $approval;
                }
            }

            $this->questions = [];
            foreach (Json::records($director->get('conversations', ['limit' => 200])['conversations'] ?? null) as $conversation) {
                $attention = $conversation['attention'] ?? null;
                if (! is_array($attention) || ($conversation['archived_at'] ?? null) !== null) {
                    continue;
                }
                $attention = Json::record($attention);
                $linked = Json::string($attention['action_approval_id'] ?? null);
                if ($linked !== '' && isset($this->approvals[$linked])) {
                    continue; // shown as the approval itself
                }
                if (Json::string($conversation['conversation_id'] ?? null) === '') {
                    continue;
                }
                $this->questions[] = [
                    'id' => Json::string($conversation['conversation_id'] ?? null),
                    'title' => Json::string($conversation['title'] ?? null, 'Conversation'),
                    'question' => Json::string($attention['question'] ?? null),
                ];
            }
            $this->loaded = true;
            $this->backlog = $this->backlogWaitingOnYou($director);
        });

        if ($this->selected !== null && ! isset($this->approvals[$this->selected])) {
            $this->selected = null;
        }
    }

    public function openBacklogItem(string $projectId, string $itemId): void
    {
        $this->navigate('/projects/'.rawurlencode($projectId), ['item' => $itemId]);
    }

    /**
     * Every project's open backlog items with an owner blocker. A Director without the backlog answers 404 for it,
     * which is the same as nothing waiting.
     *
     * @return list<array{project_id: string, project: string, item_id: string, title: string, reason: string}>
     */
    private function backlogWaitingOnYou(DirectorClient $director): array
    {
        $waiting = [];
        $this->backlogProblem = '';
        foreach (Json::records($director->get('projects')['projects'] ?? null) as $project) {
            $projectId = Json::string($project['project_id'] ?? null);
            if ($projectId === '') {
                continue;
            }
            try {
                $items = Json::records($director->get('projects/'.rawurlencode($projectId).'/backlog/needs_you')['items'] ?? null);
            } catch (DirectorRefused $refused) {
                if ($refused->status !== 404) {
                    $this->backlogProblem = 'One project\'s backlog could not be read; the rest is shown.';
                }

                continue;
            } catch (DirectorUnavailable) {
                $this->backlogProblem = 'One project\'s backlog could not be reached; the rest is shown.';

                continue;
            }
            foreach ($items as $item) {
                $waiting[] = [
                    'project_id' => $projectId,
                    'project' => Json::string($project['name'] ?? null, $projectId),
                    'item_id' => Json::string($item['item_id'] ?? null),
                    'title' => Json::string($item['title'] ?? null),
                    'reason' => Json::string($item['blocked_reason'] ?? null),
                ];
            }
        }

        return $waiting;
    }

    public function select(string $id): void
    {
        $this->selected = isset($this->approvals[$id]) ? $id : null;
    }

    public function closeDetail(): void
    {
        $this->selected = null;
    }

    public function decide(string $id, string $decision): void
    {
        if (! isset($this->approvals[$id]) || ! in_array($decision, ['approve', 'deny'], true)) {
            return;
        }

        try {
            $this->director()->write('action-approvals/'.rawurlencode($id).'/decide', ['decision' => $decision], $this->keyFor('decide:'.$id.':'.$decision));
        } catch (DirectorRefused $refused) {
            $this->answered('decide:'.$id.':'.$decision);
            if ($refused->status === 409) {
                $this->selected = null;
                $this->refresh();
                $this->problem = 'This request was already decided, or it no longer waits on you.';

                return;
            }

            $this->problem = $refused->is('owner_only')
                ? 'The Director does not accept approval decisions from this credential. Decide this one in the desktop app.'
                : $refused->explain();

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the decision may not have arrived. Deciding again is safe.';

            return;
        }

        $this->answered('decide:'.$id.':'.$decision);

        $this->selected = null;
        $this->refresh();
    }

    public function render(): View
    {
        $selected = $this->selected !== null ? ($this->approvals[$this->selected] ?? null) : null;

        return view('native.waiting', [
            'selectedApproval' => $selected,
            'command' => $selected !== null ? ActionApprovals::command($selected) : '',
            'count' => count($this->approvals) + count($this->questions) + count($this->backlog),
        ]);
    }
}
