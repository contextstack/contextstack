<?php

namespace App\Native;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use Native\Mobile\Attributes\On;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * The owner's release actions on a hosted install or rollback approval (docs/design/hosted-release-client.md):
 * deciding it and clearing a stuck request each ask for this device's own unlock first, as member and budget changes
 * do. Clearing asks for a confirmation before that, since a node worker already inside the installer may still
 * finish. After approval the screen follows the request and reads `GET conversations/{id}/smoke` for the node's last
 * smoke result.
 *
 * @mixin DirectorScreen
 */
trait ActsOnReleases
{
    /**
     * The release action waiting on the device unlock.
     *
     * @var array{action: string, id: string, value: string}|array{}
     */
    public array $pendingUnlock = [];

    /** The approval whose Clear was tapped, waiting for the person to confirm it. */
    public ?string $confirmingClear = null;

    /**
     * The node's last smoke outcome (`last_smoke`) for the selected release request's conversation, or [] when none
     * was reported or this Director does not serve it.
     *
     * @var array<string, mixed>
     */
    public array $lastSmoke = [];

    public string $notice = '';

    abstract public function refresh(): void;

    abstract protected function sendDecision(string $id, string $decision): void;

    /**
     * Whether to offer Clear: an owner (the approval's `actions.clear`, or the owner flag on an older Director) and a
     * request the Director would still clear.
     *
     * @param  array<string, mixed>  $approval
     */
    public function mayClear(array $approval): bool
    {
        if (! ReleaseApprovals::isRelease($approval) || ! ReleaseApprovals::clearable($approval)) {
            return false;
        }
        $identity = $this->identity();

        return self::action($approval, 'clear') ?? ($identity !== null && ($identity->owner || $identity->legacy()));
    }

    public function openConversation(string $id): void
    {
        $this->navigate('/conversations/'.rawurlencode($id));
    }

    public function askClear(string $id): void
    {
        $this->problem = '';
        $this->confirmingClear = $id;
    }

    public function cancelClear(): void
    {
        $this->confirmingClear = null;
    }

    public function confirmClear(): void
    {
        $id = $this->confirmingClear;
        $this->confirmingClear = null;
        if ($id !== null) {
            $this->askUnlock('release-clear', $id, '');
        }
    }

    /**
     * Acknowledge that a failed rollback is still waiting on jobs; intake stays frozen until it finishes.
     */
    public function acknowledgeRollback(string $id): void
    {
        $this->answerApproval($id, 'acknowledge');
    }

    protected function askUnlock(string $action, string $id, string $value): void
    {
        $this->problem = '';
        $this->pendingUnlock = ['action' => $action, 'id' => $id, 'value' => $value];
        Biometrics::prompt()->id($action)->prompt();
    }

    #[On(Completed::class)]
    public function unlocked(bool $success, ?string $id = null): void
    {
        if ($this->pendingUnlock === [] || $id !== $this->pendingUnlock['action']) {
            return;
        }
        $pending = $this->pendingUnlock;
        $this->pendingUnlock = [];
        if (! $success) {
            $this->problem = 'Not unlocked, so nothing was sent.';

            return;
        }

        match ($pending['action']) {
            'release-decide' => $this->sendDecision($pending['id'], $pending['value']),
            'release-clear' => $this->clearRelease($pending['id']),
            default => null,
        };
    }

    private function clearRelease(string $id): void
    {
        $key = 'clear:'.$id;
        try {
            $this->director()->write('action-approvals/'.rawurlencode($id).'/clear', [], $this->keyFor($key));
        } catch (DirectorRefused $refused) {
            $this->answered($key);
            $words = $this->refused($refused);
            if ($refused->status !== 401) {
                $this->refresh();
                $this->problem = $refused->status === 409
                    ? 'It can no longer be cleared: it finished, or it was cleared already. The screen now shows what is current.'
                    : $words;
            }

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the clear may not have arrived. Clearing again is safe.';

            return;
        }

        $this->answered($key);
        $this->refresh();
        $this->notice = 'Cleared. Check the live release on the node before asking for another switch.';
    }

    /**
     * The node's last smoke result for an approved release request's conversation. A Director without the route, or
     * a conversation that is no longer a hosted lead, answers 404: nothing to show.
     *
     * @param  array<string, mixed>  $approval
     */
    protected function readSmoke(array $approval): void
    {
        $conversation = Json::string($approval['conversation_id'] ?? null);
        if (! ReleaseApprovals::isRelease($approval) || ($approval['status'] ?? null) !== 'approved' || $conversation === '') {
            $this->lastSmoke = [];

            return;
        }

        try {
            $answer = $this->director()->get('conversations/'.rawurlencode($conversation).'/smoke');
            $this->lastSmoke = Json::record($answer['last_smoke'] ?? null);
        } catch (DirectorRefused) {
            $this->lastSmoke = [];
        } catch (DirectorUnavailable) {
            // Keep what was shown; the next refresh tries again.
        }
    }
}
