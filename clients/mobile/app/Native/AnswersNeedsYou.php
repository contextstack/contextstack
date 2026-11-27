<?php

namespace App\Native;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;

/**
 * Answering a needs-you item with the option the person pressed (docs/design/hosted-needs-you-client.md): an approval's
 * choice goes to `action-approvals/{id}/answer`, a backlog item's to `projects/{p}/backlog/{id}/answer` with the
 * revision shown. Each keeps one Idempotency-Key until answered. A 403 or 404 means it is no longer this member's to
 * answer (or gone), a 409 that it changed; either way the screen reads it again.
 *
 * @mixin DirectorScreen
 */
trait AnswersNeedsYou
{
    abstract public function refresh(): void;

    public function answerApproval(string $approvalId, string $choice): void
    {
        $this->sendAnswer('action-approvals/'.rawurlencode($approvalId).'/answer', ['choice' => $choice], 'answer:'.$approvalId.':'.$choice);
    }

    public function answerBacklog(string $projectId, string $itemId, int $revision, string $choice): void
    {
        $this->sendAnswer('projects/'.rawurlencode($projectId).'/backlog/'.rawurlencode($itemId).'/answer',
            ['choice' => $choice, 'revision' => $revision], 'answer:'.$projectId.':'.$itemId.':'.$revision.':'.$choice);
    }

    /**
     * Called after a 409 once the screen has read what is current; a screen that can say more overrides it.
     */
    protected function answerConflicted(): void {}

    /**
     * @param  array<string, mixed>  $body
     */
    private function sendAnswer(string $path, array $body, string $action): void
    {
        try {
            $this->director()->write($path, $body, $this->keyFor($action));
        } catch (DirectorRefused $refused) {
            $this->answered($action);
            if ($refused->status === 401) {
                $this->refused($refused);

                return;
            }
            $this->refused($refused);
            $this->refresh();
            $this->problem = match (true) {
                in_array($refused->status, [403, 404], true) => 'This is no longer yours to answer, or it is gone. The screen now shows what is current.',
                $refused->status === 409 => 'It was already answered, or it changed. The screen now shows what it says.',
                default => $refused->explain(),
            };
            if ($refused->status === 409) {
                $this->answerConflicted();
            }

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the answer may not have arrived. Answering again is safe.';

            return;
        }

        $this->answered($action);
        $this->problem = '';
        $this->refresh();
    }
}
