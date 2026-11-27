<?php

namespace App\Livewire;

use App\Client\BrowserApproval;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\ReleaseApproval;
use Flux\Flux;
use Livewire\Attributes\Locked;

/**
 * Action approvals on a page (CLIENT-API.md "Action approvals"): an agent asked for something beyond its sandbox, a
 * second model said go or no-go, and a no-go (or a hard line) waits on the owner. The page holds the records it
 * shows, lets the owner approve or deny one that is `needs_owner`, and fetches a record again after a decision and
 * when the stream says it changed: a `change` of kind `action_approval` carries only {id, job_id, revision}.
 */
abstract class ApprovalsPage extends DirectorPage
{
    /** @var array<string, array<string, mixed>> approval id => record */
    #[Locked]
    public array $approvals = [];

    /**
     * Replace the held approvals with a list query (`status`, `job`).
     *
     * @param  array<string, string>  $query
     */
    protected function loadApprovals(array $query = []): void
    {
        $answer = $this->fetchApprovals('action-approvals', $query);
        if ($answer === null) {
            return;
        }
        $this->approvals = [];
        foreach ((array) ($answer['action_approvals'] ?? []) as $record) {
            if (is_array($record) && is_string($record['id'] ?? null)) {
                $this->approvals[$record['id']] = $record;
            }
        }
    }

    protected function loadApproval(string $id): void
    {
        $record = $this->fetchApprovals('action-approvals/'.rawurlencode($id));
        if ($record !== null && is_string($record['id'] ?? null)) {
            $this->approvals[$record['id']] = $record;
        }
    }

    public function decideApproval(string $id, string $decision): void
    {
        if (! isset($this->approvals[$id]) || ! in_array($decision, ['approve', 'deny'], true)) {
            return;
        }

        try {
            $this->director()->write('action-approvals/'.rawurlencode($id).'/decide', ['decision' => $decision], DirectorClient::newKey());
            $this->problem = '';
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->status === 409
                ? __('This request was already decided, or it no longer waits on you. It shows its latest state below.')
                : $refused->explain();
            if ($refused->status !== 409) {
                // 401: nothing stays on screen. 403/404: the role or assignment changed; ask again and refresh.
                $this->refused($refused);
            }
        } catch (DirectorUnavailable|NotConnected $failure) {
            $this->problem = $failure->getMessage();
        }

        $this->loadApproval($id);
        $this->explainBrowserExpiry($id);
    }

    /**
     * A browser action decided or answered after `expires_at` gets a 409: the Director expires it in the same
     * transaction. The page has read it again by now; when it reads `expired`, say so instead of the generic conflict.
     */
    private function explainBrowserExpiry(string $id): void
    {
        $approval = $this->approvals[$id] ?? null;
        if ($this->problem !== '' && BrowserApproval::is($approval) && ($approval['status'] ?? null) === 'expired') {
            $this->problem = __('The window to decide this browser action ended before your decision reached the Director. It expired, and nothing was sent.');
        }
    }

    /**
     * Clear a stuck release request (CLIENT-API.md "Hosted release approvals"; owner-only `POST
     * action-approvals/{id}/clear`). Clearing revokes the claim at the Director; a node worker already inside the
     * installer may still finish, so the page asks the owner to check the live release before retrying.
     */
    public function clearApproval(string $id): void
    {
        if (! ReleaseApproval::is($this->approvals[$id] ?? null)) {
            return;
        }

        Flux::modal('clear-release-'.$id)->close();
        $this->releaseWrite($id, 'clear', [], __('This request already ended or changed. It shows its latest state below.'));
    }

    /**
     * Acknowledge a failed install whose rollback still waits on running jobs (`answer` with `acknowledge`). Intake
     * stays frozen until the previous release is restored.
     */
    public function acknowledgeRelease(string $id): void
    {
        if (! ReleaseApproval::is($this->approvals[$id] ?? null)) {
            return;
        }

        $this->releaseWrite($id, 'answer', ['choice' => 'acknowledge'], __('This was already acknowledged, or the rollback finished. It shows its latest state below.'));
    }

    /**
     * Answer a browser action approval as a needs-you item (`answer` with `go` or `no_go`), only when its record says
     * `actions.answer`. A decision is not its execution: an approved proposal's follow-up job reports separately.
     */
    public function answerBrowserApproval(string $id, string $choice): void
    {
        $approval = $this->approvals[$id] ?? null;
        if (! BrowserApproval::is($approval) || ! in_array($choice, ['go', 'no_go'], true) || ! BrowserApproval::answerable($approval)) {
            return;
        }

        $this->releaseWrite($id, 'answer', ['choice' => $choice], __('This request was already decided, or it no longer waits on you. It shows its latest state below.'));
        $this->explainBrowserExpiry($id);
    }

    /**
     * @param  array<string, string>  $body
     */
    private function releaseWrite(string $id, string $operation, array $body, string $conflict): void
    {
        try {
            $answer = $this->director()->write('action-approvals/'.rawurlencode($id).'/'.$operation, $body, DirectorClient::newKey());
            $this->problem = '';
            if (is_string($answer->body['id'] ?? null) && $answer->body['id'] === $id) {
                $this->approvals[$id] = $answer->body;
            }
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->status === 409 ? $conflict : $refused->explain();
            if ($refused->status !== 409) {
                $this->refused($refused);
            }
        } catch (DirectorUnavailable|NotConnected $failure) {
            $this->problem = $failure->getMessage();
        }

        $this->loadApproval($id);
    }

    /**
     * Fetch each approval the stream says changed and this page shows (followsApproval), once per batch; hand every
     * other change to the page as usual.
     *
     * @param  list<mixed>  $changes
     * @param  list<mixed>  $runEvents
     */
    public function applyStream(array $changes, array $runEvents = []): void
    {
        $changed = [];
        $rest = [];
        foreach ($changes as $change) {
            if (is_array($change) && ($change['kind'] ?? null) === 'action_approval') {
                if (is_string($change['id'] ?? null) && $this->followsApproval($change)) {
                    $changed[$change['id']] = true;
                }

                continue;
            }
            $rest[] = $change;
        }
        foreach (array_keys($changed) as $id) {
            $this->loadApproval((string) $id);
        }
        parent::applyStream($rest, $runEvents);
    }

    /**
     * Whether this page shows the approval an `action_approval` change names. By default, only one it holds.
     *
     * @param  array<string, mixed>  $change
     */
    protected function followsApproval(array $change): bool
    {
        return isset($this->approvals[(string) $change['id']]);
    }

    /**
     * The held approvals that wait on the owner, oldest first.
     *
     * @return list<array<string, mixed>>
     */
    public function approvalsWaiting(): array
    {
        $waiting = array_values(array_filter($this->approvals, fn (array $approval): bool => ($approval['status'] ?? null) === 'needs_owner'));
        usort($waiting, fn (array $a, array $b): int => strcmp((string) ($a['created_at'] ?? ''), (string) ($b['created_at'] ?? '')));

        return $waiting;
    }

    /**
     * A GET for approvals that never takes the page down: approvals sit beside what a page is about, so a refusal
     * shows as the page's problem without counting as the page's own refusal, and a 404 (no such approval, or a
     * Director without approvals) shows nothing.
     *
     * @param  array<string, string>  $query
     * @return array<string, mixed>|null
     */
    private function fetchApprovals(string $path, array $query = []): ?array
    {
        try {
            return $this->director()->get($path, $query);
        } catch (DirectorRefused $refused) {
            if ($refused->status !== 404) {
                $this->problem = $refused->explain();
            }
        } catch (DirectorUnavailable|NotConnected $failure) {
            $this->problem = $failure->getMessage();
        }

        return null;
    }

    protected function signOut(): void
    {
        $this->approvals = [];
        parent::signOut();
    }
}
