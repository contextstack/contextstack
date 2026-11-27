<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\ActionApprovals;
use App\Native\ActsOnReleases;
use App\Native\AnswersNeedsYou;
use App\Native\BrowserActions;
use App\Native\DirectorScreen;
use App\Native\ReleaseApprovals;
use App\Native\ShowsBrowserScreenshot;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

/**
 * What waits on you: action approvals the reviewer passed to the owner, and conversations whose lead asked a
 * question. On a phone the list and the selected item take turns; from `md` width (an iPad) they sit side by side.
 */
class Waiting extends DirectorScreen
{
    use ActsOnReleases;
    use AnswersNeedsYou;
    use ShowsBrowserScreenshot;

    /** @var array<string, array<string, mixed>> approval id => record, oldest first */
    public array $approvals = [];

    /**
     * Conversations whose lead asks, or whose release rollback waits on jobs (`approval_id` set, `rollback` true).
     *
     * @var list<array{id: string, title: string, question: string, assignee_member_id: string, approval_id: string, rollback: bool}>
     */
    public array $questions = [];

    /** @var list<array{project_id: string, project: string, item_id: string, title: string, reason: string}> backlog items blocked on you */
    public array $backlog = [];

    public ?string $selected = null;

    /** Said when some project's backlog could not be read, while the rest of the list still shows. */
    public string $backlogProblem = '';

    public bool $loaded = false;

    /** Opened from a needs-you link (pushed, with a back button), not as the Waiting tab. */
    public bool $linked = false;

    public function mount(): void
    {
        $this->refresh();
        // Opened from a needs-you link: show that approval, even when it is not in the needs_owner list.
        $wanted = Json::nullableString($this->data('approval'));
        if ($wanted !== null) {
            $this->linked = true;
            $this->selected = $wanted;
            $this->keepSelected();
            $this->followSelected();
        }
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
                // A failed rollback still waiting on jobs: its only option is to acknowledge it (CLIENT-API.md).
                $options = array_column(Json::records($attention['options'] ?? null), 'id');
                $this->questions[] = [
                    'id' => Json::string($conversation['conversation_id'] ?? null),
                    'title' => Json::string($conversation['title'] ?? null, 'Conversation'),
                    'question' => Json::string($attention['question'] ?? null),
                    'assignee_member_id' => Json::string($attention['assignee_member_id'] ?? null),
                    'approval_id' => $linked,
                    'rollback' => $linked !== '' && $options === ['acknowledge'],
                ];
            }
            $this->loaded = true;
            $this->backlog = $this->backlogWaitingOnYou($director);
        });

        $this->keepSelected();
        $this->followSelected();
    }

    /**
     * A selected release request that has been approved: read the node's last smoke result beside it. A selected
     * browser action: read its screenshot (once).
     */
    private function followSelected(): void
    {
        $approval = $this->selected !== null ? ($this->approvals[$this->selected] ?? []) : [];
        $this->readSmoke($approval);
        $this->loadScreenshot($approval);
    }

    /**
     * Open an approval that is not in the needs_owner list (a failed rollback still waiting on jobs).
     */
    public function openApproval(string $id): void
    {
        $this->selected = $id;
        $this->keepSelected();
        $this->followSelected();
    }

    /**
     * The selected approval stays shown while the Director still shows it to this credential, read on its own when
     * it has left the needs_owner list (decided elsewhere, or opened from a link).
     */
    private function keepSelected(): void
    {
        if ($this->selected === null || isset($this->approvals[$this->selected])) {
            return;
        }
        $id = $this->selected;
        $this->selected = null;
        $this->attempt(function (DirectorClient $director) use ($id): void {
            $approval = $director->find('action-approvals/'.rawurlencode($id));
            $approval = $approval === null ? [] : Json::record($approval['action_approval'] ?? $approval);
            if (Json::string($approval['id'] ?? null) === $id) {
                $this->approvals[$id] = $approval;
                $this->selected = $id;
            }
        });
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
        $this->confirmingClear = null;
        $this->notice = '';
        $this->followSelected();
    }

    public function closeDetail(): void
    {
        $this->selected = null;
        $this->confirmingClear = null;
    }

    /**
     * Approve or deny. A release (install or rollback) or a browser action asks for the device unlock first.
     */
    public function decide(string $id, string $decision): void
    {
        $approval = $this->approvals[$id] ?? null;
        if ($approval === null || ! in_array($decision, ['approve', 'deny'], true) || ! $this->mayDecideApproval($approval)
            || ($decision === 'approve' && ! ActionApprovals::decidable($approval))) {
            return;
        }
        if (ReleaseApprovals::isRelease($approval) || BrowserActions::is($approval)) {
            $this->askUnlock('release-decide', $id, $decision);

            return;
        }

        $this->sendDecision($id, $decision);
    }

    protected function sendDecision(string $id, string $decision): void
    {
        $release = ReleaseApprovals::isRelease($this->approvals[$id] ?? []);
        // A browser action also stays shown, so its approved or denied state follows the decision.
        $keep = $release || BrowserActions::is($this->approvals[$id] ?? []);
        try {
            $this->director()->write('action-approvals/'.rawurlencode($id).'/decide', ['decision' => $decision], $this->keyFor('decide:'.$id.':'.$decision));
        } catch (DirectorRefused $refused) {
            $this->answered('decide:'.$id.':'.$decision);
            if ($refused->status === 401) {
                $this->refused($refused);

                return;
            }
            if (in_array($refused->status, [403, 404], true) && ! $refused->is('owner_only')) {
                $this->refused($refused);
                $this->selected = null;
                $this->refresh();
                $this->problem = 'This request is no longer yours to decide, or it is gone. The list now shows what waits on you.';

                return;
            }
            if ($refused->status === 409) {
                // A browser action decided after its window is expired by the Director in the same step: read it
                // again and show that state.
                $this->selected = $keep && ! $release ? $id : null;
                unset($this->approvals[$id]);
                $this->refresh();
                $now = $this->approvals[$id] ?? [];
                $this->problem = BrowserActions::is($now) && ($now['status'] ?? null) === 'expired'
                    ? 'The decision window had ended, so the Director expired this request. Nothing was submitted.'
                    : 'This request was already decided, or it no longer waits on you.';

                return;
            }

            $this->problem = $refused->is('owner_only')
                ? 'The Director does not accept approval decisions from this credential.'
                : $refused->explain();

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the decision may not have arrived. Deciding again is safe.';

            return;
        }

        $this->answered('decide:'.$id.':'.$decision);

        // A release stays shown, so its waiting state and then its smoke result follow the approval.
        $this->selected = $keep ? $id : null;
        $this->refresh();
        if ($release && $decision === 'approve') {
            $this->notice = 'Approved. The node switches releases and runs the smoke trial; if the trial fails it restores the previous release.';
        }
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
