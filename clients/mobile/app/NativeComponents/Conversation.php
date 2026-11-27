<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\IdentityStore;
use App\Client\Json;
use App\Native\AnswersNeedsYou;
use App\Native\BrowserActions;
use App\Native\DirectorScreen;
use App\Native\NeedsYou;
use App\Native\ReleaseApprovals;
use App\Native\ShowsBrowserScreenshot;
use App\Native\When;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Attributes\Poll;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * One conversation's thread (CLIENT-API.md §11): every turn with its reply, newest first so the latest reply and
 * the message box are what the screen opens on; post a message (queued behind a running turn, from any device),
 * stop the conversation or let it go on. The handlers are named stopConversation/resumeConversation because
 * NativeComponent::stop() ends the screen's run loop, and back() calls it: overriding it sent a conversation stop
 * on every back press and left the screen stuck. Each message keeps one Idempotency-Key until the Director has recorded it,
 * so sending again after a lost answer replays it instead of posting it twice.
 */
class Conversation extends DirectorScreen
{
    use AnswersNeedsYou;
    use ShowsBrowserScreenshot;

    public const MaxMessage = 200000;

    public string $conversationId = '';

    /** @var array<string, mixed> */
    public array $conversation = [];

    /** @var list<array<string, mixed>> newest first */
    public array $turns = [];

    public string $message = '';

    /**
     * The approval the attention links, read on its own so its kind is known before a choice is offered: only an
     * owner decides a land approval. [] when there is none or this credential may not see it.
     *
     * @var array<string, mixed>
     */
    public array $linkedApproval = [];

    public string $notice = '';

    public string $postKey = '';

    public bool $loaded = false;

    /**
     * A browser action whose answer came back 409: read again, it is the record the Director now holds (expired when
     * its window had ended). [] otherwise.
     *
     * @var array<string, mixed>
     */
    public array $browserOutcome = [];

    /** The browser action approval an answer is on its way for, or ''. */
    private string $answeringBrowser = '';

    /** A release approval's Go or No-go waiting on the device unlock. */
    public string $pendingChoice = '';

    /** Archive was tapped and waits for the person to confirm it. */
    public bool $confirmingArchive = false;

    public function mount(): void
    {
        $this->conversationId = Json::string($this->param('id'));
        $this->postKey = DirectorClient::newKey();
        $this->refresh();
    }

    public function onResume(): void
    {
        $this->refresh();
    }

    /**
     * There is no event stream on the phone yet, so the open thread checks the conversation record every 5 seconds
     * and fetches the whole thread (every turn, always) only when the record's revision has moved.
     */
    #[Poll(5000)]
    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $path = 'conversations/'.rawurlencode($this->conversationId);
            if ($this->loaded) {
                $answer = $director->get($path);
                $conversation = Json::record($answer['conversation'] ?? $answer);
                if (($conversation['revision'] ?? null) === ($this->conversation['revision'] ?? null) && ! $this->inProgress()) {
                    return;
                }
            }
            $this->reload($director);
        });
    }

    /**
     * Fetch the whole thread, whatever the revision says (after this screen posts or stops).
     */
    private function reload(DirectorClient $director): void
    {
        $thread = $director->get('conversations/'.rawurlencode($this->conversationId).'/thread');
        $this->conversation = Json::record($thread['conversation'] ?? null);
        $turns = Json::records($thread['turns'] ?? null);
        usort($turns, fn (array $a, array $b): int => self::seq($b) <=> self::seq($a));
        $this->turns = $turns;
        $this->linkedApproval = $this->readLinkedApproval($director);
        $this->loadScreenshot($this->attention());
        $this->loaded = true;
    }

    /**
     * @return array<string, mixed>
     */
    private function readLinkedApproval(DirectorClient $director): array
    {
        // The attention says the approval's kind (or what may be done) itself on newer Directors: no read needed.
        $attention = $this->attention();
        $id = NeedsYou::links($attention)['approval'] ?? null;
        if ($id === null || isset($attention['kind']) || self::action($attention, 'answer') !== null) {
            return [];
        }

        try {
            $approval = $director->find('action-approvals/'.rawurlencode($id));
        } catch (DirectorRefused|DirectorUnavailable) {
            return [];
        }

        return $approval === null ? [] : Json::record($approval['action_approval'] ?? $approval);
    }

    /**
     * A turn still going: its `waiting` is worked out when read and moves no revision, so the thread is read again.
     */
    private function inProgress(): bool
    {
        foreach ($this->turns as $turn) {
            if (in_array($turn['state'] ?? null, ['queued', 'running', 'summarising'], true)) {
                return true;
            }
        }

        return false;
    }

    private function reloadNow(): void
    {
        $this->attempt(fn (DirectorClient $director) => $this->reload($director));
    }

    public function post(): void
    {
        $message = trim($this->message);
        if ($message === '') {
            $this->problem = 'Write a message first.';

            return;
        }
        if (strlen($message) > self::MaxMessage) {
            $this->problem = 'That message is too long to send.';

            return;
        }

        try {
            $answer = $this->director()->write('conversations/'.rawurlencode($this->conversationId).'/turns', ['message' => $message], $this->postKey);
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the message may not have arrived. Sending again is safe.';

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->postKey = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return;
        }

        $this->postKey = DirectorClient::newKey();
        $this->message = '';
        $this->problem = '';
        $this->notice = Json::string(Json::record($answer->body['turn'] ?? null)['state'] ?? null) === 'queued'
            ? 'Queued behind the current reply.'
            : '';
        $this->reloadNow();
    }

    public function stopConversation(): void
    {
        $this->control('stop', 'The Director could not be reached, so the conversation was not stopped.');
    }

    public function resumeConversation(): void
    {
        $this->control('resume', 'The Director could not be reached, so the conversation did not go on.');
    }

    /**
     * The conversation's needs-you item, or [] when nothing waits (CLIENT-API.md §11.2, §11.7).
     *
     * @return array<string, mixed>
     */
    public function attention(): array
    {
        return Json::record($this->conversation['attention'] ?? null);
    }

    /**
     * Whether this credential may press one of the attention's options: an approval's choice needs `approve_action`
     * and the assignment (or an owner's takeover), a backlog item's `edit_backlog`. The Director still decides.
     */
    public function mayAnswer(): bool
    {
        $attention = $this->attention();
        $told = self::action($attention, 'answer');
        if ($told !== null) {
            return $told;
        }
        $links = NeedsYou::links($attention);
        if (isset($links['approval'])) {
            if (isset($attention['kind'])) {
                return $this->mayDecideApproval($attention);
            }

            return $this->linkedApproval !== [] && $this->mayDecideApproval(array_merge($attention, ['kind' => $this->linkedApproval['kind'] ?? null]));
        }

        return isset($links['backlog']) && Json::string($attention['project_id'] ?? null) !== '' && $this->can('edit_backlog');
    }

    /**
     * Press an attention option: an approval's choice, or a backlog item's against the revision shown.
     */
    public function choose(string $choice): void
    {
        $attention = $this->attention();
        $links = NeedsYou::links($attention);
        if (! $this->mayAnswer() || ! in_array($choice, array_column($this->offeredOptions(), 'id'), true)) {
            return;
        }

        if (isset($links['approval'])) {
            // Deciding an install, rollback or browser action asks for the device unlock first; acknowledging a
            // waiting rollback does not.
            if (($this->releaseAttention() || BrowserActions::is($attention)) && $choice !== 'acknowledge') {
                $this->problem = '';
                $this->pendingChoice = $choice;
                Biometrics::prompt()->id('release-answer')->prompt();

                return;
            }
            $this->sendApprovalChoice($links['approval'], $choice);

            return;
        }
        $revision = $attention['revision'] ?? null;
        $this->answerBacklog(Json::string($attention['project_id'] ?? null), $links['backlog'] ?? '', is_int($revision) ? $revision : 0, $choice);
    }

    /**
     * The attention's options this screen offers: a browser action whose window has ended offers No-go only.
     *
     * @return list<array{id: string, label: string}>
     */
    public function offeredOptions(): array
    {
        $attention = $this->attention();
        $options = NeedsYou::options($attention);
        if (BrowserActions::is($attention) && BrowserActions::ended($attention)) {
            $options = array_values(array_filter($options, fn (array $option): bool => $option['id'] !== 'go'));
        }

        return $options;
    }

    /**
     * Whether the attention links a hosted install or rollback approval.
     */
    public function releaseAttention(): bool
    {
        $attention = $this->attention();

        return ReleaseApprovals::isRelease(['kind' => $attention['kind'] ?? $this->linkedApproval['kind'] ?? null]);
    }

    /**
     * Who the thread runs under: the hosted lead's sponsor and the member its work is attributed to.
     */
    public function sponsorship(): string
    {
        $parts = [];
        $sponsor = Json::nullableString($this->conversation['sponsor_member_id'] ?? null);
        if ($sponsor !== null) {
            $parts[] = 'Sponsored by '.$this->nameOn($this->conversation, 'sponsor');
        }
        $responsible = Json::nullableString($this->conversation['responsible_member_id'] ?? null);
        if ($responsible !== null && $responsible !== $sponsor) {
            $parts[] = 'work attributed to '.$this->nameOn($this->conversation, 'responsible');
        }
        $paused = Json::string($this->conversation['paused_reason'] ?? null);
        if ($paused !== '') {
            $parts[] = self::pausedWords($paused);
        }

        return implode(' · ', $parts);
    }

    /**
     * A turn sent by another conversation (CLIENT-API.md "Peer messages"): who sent it, marked untrusted, or '' for
     * an ordinary turn. Its text is shown as plain text and carries no instructions or approvals.
     *
     * @param  array<string, mixed>  $turn
     */
    public static function peer(array $turn): string
    {
        $peer = Json::record($turn['peer'] ?? null);
        if ($peer === []) {
            return '';
        }
        $from = Json::string($peer['from_conversation'] ?? null);
        $title = Json::string($peer['from_title'] ?? null, $from);

        return 'Peer brief from '.$title.($from !== '' && $from !== $title ? ' ('.$from.')' : '').' · untrusted';
    }

    /**
     * Whether to offer adoption: a hosted lead sponsored by someone else, and a credential that may adopt. The contract
     * makes adoption owner/admin-only without naming a capability for it, so it follows `manage_members`, which only
     * owners and admins hold. The Director decides.
     */
    public function mayAdopt(): bool
    {
        $told = self::action($this->conversation, 'adopt');
        if ($told !== null) {
            return $told && $this->identity()?->memberId !== null;
        }
        $identity = $this->identity();
        $sponsor = Json::nullableString($this->conversation['sponsor_member_id'] ?? null);

        if (($this->conversation['lead'] ?? false) !== true || $identity === null || $identity->memberId === null
            || $identity->isMe($sponsor) || ! $this->can('manage_members')) {
            return false;
        }

        // An admin lists only members and viewers: a sponsor missing from that list is an owner or admin, whose
        // conversation the Director will not let an admin adopt.
        return $identity->owner || ($sponsor !== null && isset(app(IdentityStore::class)->memberNames()[$sponsor]));
    }

    /**
     * Adopt the conversation as its sponsor, after the device unlock. It revokes the lead's current credentials.
     */
    public function adopt(): void
    {
        if ($this->mayAdopt()) {
            $this->problem = '';
            Biometrics::prompt()->id('adopt')->prompt();
        }
    }

    #[On(Completed::class)]
    public function unlocked(bool $success, ?string $id = null): void
    {
        if ($id === 'release-answer') {
            $this->releaseUnlocked($success);

            return;
        }
        if ($id !== 'adopt') {
            return;
        }
        $me = $this->identity()?->memberId;
        if (! $success || $me === null || ! $this->mayAdopt()) {
            $this->problem = $success ? '' : 'Not unlocked, so the conversation was not adopted.';

            return;
        }

        try {
            $this->director()->put('conversations/'.rawurlencode($this->conversationId), ['sponsor_member_id' => $me], $this->keyFor('adopt:'.$me), 'PATCH');
        } catch (DirectorRefused $refused) {
            $this->answered('adopt:'.$me);
            $words = $this->refused($refused);
            if ($refused->status !== 401) {
                $this->reloadNow();
                $this->problem = $refused->status === 409 ? 'The Director refused the adoption: '.$refused->detail : $words;
            }

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the adoption may not have arrived. Adopting again is safe.';

            return;
        }

        $this->answered('adopt:'.$me);
        $this->problem = '';
        $this->reloadNow();
    }

    private function releaseUnlocked(bool $success): void
    {
        $choice = $this->pendingChoice;
        $this->pendingChoice = '';
        $approval = NeedsYou::links($this->attention())['approval'] ?? null;
        if ($choice === '' || $approval === null) {
            return;
        }
        if (! $success) {
            $this->problem = 'Not unlocked, so nothing was sent.';

            return;
        }
        $this->sendApprovalChoice($approval, $choice);
    }

    private function sendApprovalChoice(string $approvalId, string $choice): void
    {
        $this->browserOutcome = [];
        $this->answeringBrowser = BrowserActions::is($this->attention()) ? $approvalId : '';
        $this->answerApproval($approvalId, $choice);
        $this->answeringBrowser = '';
    }

    /**
     * A browser action answered after its window: the Director expired it in the same step. Show what it holds now.
     */
    protected function answerConflicted(): void
    {
        if ($this->answeringBrowser === '') {
            return;
        }
        try {
            $found = $this->director()->find('action-approvals/'.rawurlencode($this->answeringBrowser));
        } catch (DirectorRefused|DirectorUnavailable) {
            return;
        }
        $this->browserOutcome = $found === null ? [] : Json::record($found['action_approval'] ?? $found);
        if (($this->browserOutcome['status'] ?? null) === 'expired') {
            $this->problem = 'The decision window had ended, so the Director expired this request. Nothing was submitted.';
        }
    }

    /**
     * Whether to offer a control on this thread: the capability, and the thread is this member's (or this credential
     * manages members).
     */
    public function drives(string $capability): bool
    {
        return self::action($this->conversation, 'control') ?? ($this->can($capability) && $this->mine($this->conversation));
    }

    /**
     * Why a conversation is paused (`paused_reason`), in words.
     */
    public static function pausedWords(string $reason): string
    {
        return match ($reason) {
            'sponsor_ineligible' => 'Paused: its sponsor can no longer run work. An owner or admin can adopt it.',
            default => 'Paused: '.str_replace('_', ' ', $reason).'.',
        };
    }

    /**
     * Whether to offer archive and unarchive: the record's `actions.archive` when the Director sends it, otherwise
     * `control_work` on this member's thread.
     */
    public function mayArchive(): bool
    {
        return self::action($this->conversation, 'archive') ?? ($this->can('control_work') && $this->mine($this->conversation));
    }

    public function archived(): bool
    {
        return Json::string($this->conversation['archived_at'] ?? null) !== '';
    }

    /**
     * Archive asks first: the conversation leaves the lists and takes no messages until it is unarchived.
     */
    public function askArchive(): void
    {
        if ($this->mayArchive() && ! $this->archived()) {
            $this->problem = '';
            $this->confirmingArchive = true;
        }
    }

    public function cancelArchive(): void
    {
        $this->confirmingArchive = false;
    }

    public function archiveConversation(): void
    {
        $this->confirmingArchive = false;
        if ($this->mayArchive()) {
            $this->archiveState('archive', 'Archived. It no longer shows in Conversations, and takes messages again once unarchived.');
        }
    }

    public function unarchiveConversation(): void
    {
        if ($this->mayArchive()) {
            $this->archiveState('unarchive', 'Unarchived.');
        }
    }

    /**
     * `POST conversations/{id}/archive` or `/unarchive` (CLIENT-API.md §11.8), keyed until the Director answers.
     */
    private function archiveState(string $action, string $done): void
    {
        $key = $action.':'.$this->conversationId;
        try {
            $this->director()->write('conversations/'.rawurlencode($this->conversationId).'/'.$action, [], $this->keyFor($key));
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the change may not have arrived. Trying again is safe.';

            return;
        } catch (DirectorRefused $refused) {
            $this->answered($key);
            $words = $this->refused($refused);
            if ($refused->status !== 401) {
                $this->reloadNow();
                $this->problem = $words;
            }

            return;
        }

        $this->answered($key);
        $this->problem = '';
        $this->notice = $done;
        $this->reloadNow();
    }

    public function held(): bool
    {
        return ($this->conversation['held'] ?? false) === true || ($this->conversation['state'] ?? null) === 'held';
    }

    public function title(): string
    {
        $title = Json::string($this->conversation['title'] ?? null);
        if ($title !== '') {
            return $title;
        }

        return ($this->conversation['lead'] ?? false) === true ? 'Lead' : 'Conversation';
    }

    public function subtitle(): string
    {
        $parts = array_filter([
            Json::string($this->conversation['model'] ?? null),
            Conversations::stateWords(Json::string($this->conversation['state'] ?? null)),
        ]);

        return implode(' · ', $parts);
    }

    /**
     * What sits under a turn's message: the reply, or what is happening instead.
     *
     * @param  array<string, mixed>  $turn
     */
    public static function answer(array $turn): string
    {
        $state = Json::string($turn['state'] ?? null);
        $detail = Json::string($turn['detail'] ?? null);
        $waiting = self::waiting($turn);
        if ($waiting !== '' && in_array($state, ['queued', 'running', 'summarising'], true)) {
            return $waiting;
        }

        return match ($state) {
            'done' => Json::string($turn['reply'] ?? null, '(no reply text)'),
            'queued' => 'Queued: it starts when the reply before it is done.',
            'summarising' => 'Waiting while earlier turns are summarised.',
            'running' => 'Replying…',
            'stopped' => $detail === '' || str_contains(strtolower($detail), "person's request") ? 'You stopped this turn.' : 'Stopped: '.$detail,
            'error' => 'Failed.'.($detail !== '' ? ' '.$detail : ''),
            default => $state,
        };
    }

    /**
     * What a turn is waiting on, from the Director's derived `waiting` (CLIENT-API.md §11.2), or '' when nothing:
     * its machine has not picked the job up, or the job is held.
     *
     * @param  array<string, mixed>  $turn
     */
    public static function waiting(array $turn): string
    {
        $waiting = Json::record($turn['waiting'] ?? null);

        return match (Json::string($waiting['reason'] ?? null)) {
            'node_not_pulled' => 'Waiting for '.Json::string($waiting['node'] ?? null, 'its machine').' to pick this up'
                .(Json::string($waiting['node_last_seen'] ?? null) !== '' ? ' (last heard from '.When::relative($waiting['node_last_seen']).')' : '').'…',
            'held' => Json::string($waiting['detail'] ?? null) === 'sponsor_ineligible'
                ? 'Paused: this conversation\'s sponsor can no longer run work. An owner or admin can adopt it.'
                : 'Held: '.Json::string($waiting['detail'] ?? null, 'the Director is holding this job').'.',
            default => '',
        };
    }

    /**
     * @param  array<string, mixed>  $turn
     */
    public static function failed(array $turn): bool
    {
        return in_array($turn['state'] ?? null, ['error', 'stopped'], true);
    }

    /**
     * @param  array<string, mixed>  $turn
     */
    public static function sender(array $turn): string
    {
        $peer = self::peer($turn);
        if ($peer !== '') {
            return $peer;
        }

        return Json::string($turn['posted_by_name'] ?? null, 'You');
    }

    public function render(): View
    {
        return view('native.conversation');
    }

    private function control(string $action, string $unreachable): void
    {
        try {
            $this->director()->write('conversations/'.rawurlencode($this->conversationId).'/'.$action, [], DirectorClient::newKey());
        } catch (DirectorUnavailable) {
            $this->problem = $unreachable;

            return;
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();

            return;
        }

        $this->problem = '';
        $this->reloadNow();
    }

    /**
     * @param  array<string, mixed>  $turn
     */
    private static function seq(array $turn): int
    {
        $seq = $turn['seq'] ?? 0;

        return is_int($seq) ? $seq : 0;
    }
}
