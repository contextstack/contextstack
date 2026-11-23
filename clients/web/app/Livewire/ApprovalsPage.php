<?php

namespace App\Livewire;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
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
}
