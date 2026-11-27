<?php

use App\Client\ChiefOfStaff;
use App\Client\ConversationOrder;
use App\Client\DirectorClient;
use App\Client\Identity;
use App\Client\MemberNames;
use App\Client\NeedsYou;
use App\Client\RecordActions;
use App\Client\ReleaseApproval;
use App\Client\Leads;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\LiveUpdates;
use App\Client\ModelCatalog;
use App\Client\TurnFailure;
use App\Livewire\ApprovalsPage;
use App\Livewire\Forms\BudgetForm;
use Flux\Flux;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * One conversation (CLIENT-API.md §11): the whole thread, every turn and every summary, always; summaries are shown
 * as summaries, never as turns. Post a turn (queued behind the active one, from any device), stop and resume, redo
 * a summary, and set or clear the conversation's budget (§12.2).
 *
 * Replies appear as they are written: the stream is filtered to this conversation (`GET events?conversation=`), its
 * run_events are tagged with the turn or summary, and when the turn ends its `reply` replaces the partial text.
 */
new #[Layout('layouts::client')] #[Title('Conversation')] class extends ApprovalsPage
{
    public BudgetForm $budget;

    /** Text in the composer, at most what the Director takes (200,000 bytes). */
    private const int MaxMessage = 200000;

    #[Locked]
    public string $conversationId = '';

    /** @var array<string, array{seq: int, text: string}> live output per run id */
    #[Locked]
    public array $output = [];

    /** @var array<string, array{kind: string, id: string}> which turn or summary each live run belongs to */
    #[Locked]
    public array $runOwners = [];

    public string $message = '';

    #[Locked]
    public string $postKey = '';

    #[Locked]
    public string $stopKey = '';

    #[Locked]
    public string $resumeKey = '';

    #[Locked]
    public string $redoKey = '';

    #[Locked]
    public string $archiveKey = '';

    #[Locked]
    public string $deleteKey = '';

    /** @var list<array{name: string, label: string, available: bool}> */
    #[Locked]
    public array $modelChoices = [];

    /** @var array<string, string> */
    #[Locked]
    public array $retryKeys = [];

    /** @var array<string, string> turn id => model name for "try with another model" */
    public array $retryModel = [];

    /** @var array<string, array{conversation_id: string, model: string, label: string}> */
    #[Locked]
    public array $continuedIn = [];

    /** @var array<string, string> idempotency keys for starting a continued conversation */
    #[Locked]
    public array $continueKeys = [];

    public string $notice = '';

    /** Turns shown at first and added by each "Show earlier": the page holds and renders only the newest ones. */
    public const WINDOW = 40;

    #[Locked]
    public int $window = self::WINDOW;


    /** @var list<string> conversations this one may send a brief to (`GET conversations/{id}/peer-messages`) */
    #[Locked]
    public array $peers = [];

    public string $peerTo = '';

    public string $peerText = '';

    #[Locked]
    public string $peerKey = '';

    public string $peerProblem = '';

    /** Whether this is a lead conversation, which starts lanes; its page shows them beside the thread. */
    #[Locked]
    public bool $isLead = false;

    /** The chief of staff's own conversation (ChiefOfStaff), named as the slot is; not archived or deleted while the slot is on. */
    #[Locked]
    public string $chiefName = '';

    /** @var array<string, mixed>|null the node's last smoke (`GET conversations/{id}/smoke`), when it has one */
    #[Locked]
    public ?array $lastSmoke = null;

    public function mount(string $conversation): void
    {
        $this->conversationId = $conversation;
        $this->postKey = DirectorClient::newKey();
        $this->stopKey = DirectorClient::newKey();
        $this->resumeKey = DirectorClient::newKey();
        $this->redoKey = DirectorClient::newKey();
        $this->archiveKey = DirectorClient::newKey();
        $this->deleteKey = DirectorClient::newKey();
        $this->refetch();
        abort_if($this->refusedStatus === 404, 404);
        $this->loadModelChoices();
        $this->peerKey = DirectorClient::newKey();
        $this->loadPeers();
        $held = $this->conversation();
        $this->isLead = $held !== null && Leads::is($held);
        if ($this->isLead) {
            $this->loadReleaseApprovals();
        }
        if (app(ChiefOfStaff::class)->conversationId() === $this->conversationId) {
            $this->chiefName = app(ChiefOfStaff::class)->slot()['name'];
        }
        // Set by the new-conversation page when the conversation started but its first message did not get through.
        if (is_string(session('conversation-problem'))) {
            $this->problem = session('conversation-problem');
        }
    }

    /**
     * Also tells the lanes pane when a lane this lead started changes: the conversation's stream carries those
     * intents and jobs, and this page is the one holding the stream.
     */
    public function applyStream(array $changes, array $runEvents = []): void
    {
        foreach ($changes as $change) {
            // Deleted from another device or window: this page has nothing left to show.
            if (is_array($change) && ($change['kind'] ?? null) === 'conversation' && ($change['id'] ?? null) === $this->conversationId
                && ($change['deleted'] ?? false) === true) {
                $this->leaveDeleted();

                return;
            }
        }
        foreach ($changes as $change) {
            if (is_array($change) && in_array($change['kind'] ?? null, ['intent', 'job'], true)
                && ($change['record']['lead_conversation_id'] ?? null) === $this->conversationId) {
                $this->dispatch('thread-lanes-changed');
                break;
            }
        }

        parent::applyStream($changes, $runEvents);
    }

    /**
     * Where this conversation may send briefs. Only a Director with teams has the route and only a member who can
     * drive the conversation may ask; anything else leaves the list empty and the form hidden.
     */
    private function loadPeers(): void
    {
        $identity = app(Identity::class);
        if (! is_array($identity->me()['capabilities'] ?? null) || ! $identity->can('start_work')) {
            return;
        }

        try {
            $answer = $this->director()->find('conversations/'.rawurlencode($this->conversationId).'/peer-messages');
        } catch (DirectorRefused|DirectorUnavailable) {
            return;
        }
        $this->peers = array_values(array_filter((array) ($answer['peers'] ?? []), fn ($peer): bool => is_string($peer) && $peer !== ''));
    }

    /**
     * A changed brief or destination is a different send: it gets its own key, so it is never taken for a replay.
     */
    public function updatedPeerText(): void
    {
        $this->peerKey = DirectorClient::newKey();
    }

    public function updatedPeerTo(): void
    {
        $this->peerKey = DirectorClient::newKey();
    }

    /**
     * Send a brief to another conversation (CLIENT-API.md §11.6, Peer messages): one key per send, kept until the
     * Director answers so a lost answer is retried as the same send. Over the hourly limit (429) the Director raises
     * a needs-you item on this conversation; the page shows it and sends nothing again by itself.
     */
    public function sendPeer(): void
    {
        $this->peerProblem = '';
        $this->validate([
            'peerTo' => ['required', 'in:'.implode(',', $this->peers ?: [''])],
            'peerText' => ['required', 'string', 'max:16384'],
        ]);
        if (strlen($this->peerText) > 16384) {
            $this->addError('peerText', __('A brief is at most 16 KB.'));

            return;
        }

        try {
            $this->director()->write('conversations/'.rawurlencode($this->conversationId).'/peer-messages', ['to' => $this->peerTo, 'text' => $this->peerText], $this->peerKey);
        } catch (DirectorRefused $refused) {
            $this->peerProblem = match (true) {
                $refused->status === 429 => __('That pair has sent its limit of briefs this hour. Nothing was sent, and nothing will be retried; this conversation shows it under Needs you.'),
                $refused->is('peer_not_allowed') => __('Briefs from this conversation to that one are not allowed any more.'),
                default => $refused->explain(),
            };
            if ($refused->status !== 429) {
                $this->peerKey = DirectorClient::newKey();
                $this->loadPeers();
            }
            $this->refetch();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->peerProblem = $unavailable->getMessage();

            return;
        }

        $this->peerText = '';
        $this->peerKey = DirectorClient::newKey();
        Flux::toast(variant: 'success', text: __('Brief sent. The other conversation reads it as untrusted text.'));
    }

    private function loadModelChoices(): void
    {
        $choices = $this->attempt(fn () => ModelCatalog::availableChoices($this->director()));
        if (is_array($choices)) {
            $this->modelChoices = $choices;
        }
    }

    public function refetch(): void
    {
        $answer = $this->attempt(fn () => $this->director()->get('conversations/'.rawurlencode($this->conversationId).'/thread'));
        if ($answer === null) {
            return;
        }

        $this->keep('conversation', $this->conversationId, (array) $answer['conversation']);
        foreach ((array) ($answer['turns'] ?? []) as $turn) {
            $this->keep('turn', (string) $turn['turn_id'], $turn);
        }
        foreach ((array) ($answer['summaries'] ?? []) as $summary) {
            $this->keep('summary', (string) $summary['summary_id'], $summary);
        }
        $this->afterChanges();
    }

    public function showEarlier(): void
    {
        $this->window += self::WINDOW;
        $this->refetch();
    }

    /**
     * Whether turns older than the ones held exist: the oldest held turn is not the first.
     */
    public function hasEarlier(): bool
    {
        $first = $this->turns()[0] ?? null;

        return $first !== null && (int) $first['seq'] > 1;
    }

    /**
     * Keep only what the page shows: the newest $window turns, the summaries placed among them, the jobs of those
     * turns and summaries, and the live text of runs still in progress. Everything held travels with every Livewire
     * request, and a long thread would otherwise pass Livewire's request limit (about 1MB).
     */
    private function trim(): void
    {
        $turns = $this->turns();
        $dropped = array_slice($turns, 0, max(0, count($turns) - $this->window));
        foreach ($dropped as $turn) {
            unset($this->records['turn:'.$turn['turn_id']]);
        }
        $first = (int) ($this->turns()[0]['seq'] ?? 1);

        $jobs = [];
        foreach ($this->summaries() as $summary) {
            if ((int) $summary['through_seq'] < $first - 1 && $first > 1) {
                unset($this->records['summary:'.$summary['summary_id']]);
            }
        }
        $owners = [];
        foreach ([...$this->turns(), ...$this->summaries()] as $item) {
            if (is_string($item['job_id'] ?? null)) {
                $jobs[$item['job_id']] = true;
            }
            if (! $this->finished($item)) {
                $owners[isset($item['turn_id']) ? 'turn:'.$item['turn_id'] : 'summary:'.$item['summary_id']] = true;
            }
        }
        foreach (array_keys($this->records) as $key) {
            if (str_starts_with($key, 'run:') || (str_starts_with($key, 'job:') && ! isset($jobs[substr($key, 4)]))) {
                unset($this->records[$key]);
            }
        }
        foreach ($this->runOwners as $runId => $owner) {
            if (! isset($owners[$owner['kind'].':'.$owner['id']])) {
                unset($this->runOwners[$runId], $this->output[$runId]);
            }
        }
    }

    protected function follows(array $change): bool
    {
        $kind = $change['kind'] ?? null;

        if ($kind === 'conversation') {
            return ($change['id'] ?? null) === $this->conversationId;
        }

        return in_array($kind, ['turn', 'summary', 'job'], true)
            && ($change['record']['conversation_id'] ?? null) === $this->conversationId;
    }

    /**
     * An approval this thread shows, one its attention links, or a land approval of this conversation (whose change
     * carries `conversation_id`, CLIENT-API.md "Action approvals").
     *
     * @param  array<string, mixed>  $change
     */
    protected function followsApproval(array $change): bool
    {
        return parent::followsApproval($change)
            || ($change['id'] ?? null) === $this->attentionApprovalId()
            || ($change['record']['conversation_id'] ?? null) === $this->conversationId;
    }

    /**
     * The release approvals (install, rollback) this lead conversation asked for, newest few: they stay on the page
     * after they are decided, with the node's result. The Director filters by conversation (CLIENT-API.md
     * `action-approvals?conversation=`). Only approvers can list approvals; others see the one the conversation's
     * attention links.
     */
    private function loadReleaseApprovals(): void
    {
        $identity = app(Identity::class);
        if (! $identity->legacy() && ! $identity->can('approve_action')) {
            return;
        }
        try {
            $answer = $this->director()->get('action-approvals', ['conversation' => $this->conversationId]);
        } catch (DirectorRefused|DirectorUnavailable|\App\Client\Exceptions\NotConnected) {
            return;
        }
        $mine = array_values(array_filter((array) ($answer['action_approvals'] ?? []), fn (mixed $record): bool => ReleaseApproval::is($record)
            && is_string($record['id'] ?? null) && ($record['conversation_id'] ?? null) === $this->conversationId));
        usort($mine, fn (array $a, array $b): int => strcmp((string) ($b['created_at'] ?? ''), (string) ($a['created_at'] ?? '')));
        foreach (array_slice($mine, 0, 5) as $record) {
            $this->approvals[(string) $record['id']] = $record;
        }
        $this->loadSmokeIfReported();
    }

    /**
     * A release approval came back from the Director: once the node has reported, read its last smoke too.
     */
    protected function loadApproval(string $id): void
    {
        parent::loadApproval($id);
        if (ReleaseApproval::is($this->approvals[$id] ?? null)) {
            $this->loadSmokeIfReported();
        }
    }

    /**
     * The node's last smoke (`GET conversations/{id}/smoke`), read only when one of this conversation's release
     * approvals has a reported outcome. A side read: a refusal or an unreachable Director leaves it as it was.
     */
    private function loadSmokeIfReported(): void
    {
        $reported = array_filter($this->releaseApprovals(), fn (array $approval): bool => ReleaseApproval::phase($approval) === 'reported');
        if ($reported === []) {
            return;
        }
        try {
            $answer = $this->director()->get('conversations/'.rawurlencode($this->conversationId).'/smoke');
            $this->lastSmoke = is_array($answer['last_smoke'] ?? null) ? $answer['last_smoke'] : null;
        } catch (DirectorRefused|DirectorUnavailable|\App\Client\Exceptions\NotConnected) {
            // Nothing to add: each approval already shows its own outcome.
        }
    }

    /**
     * The held release approvals of this conversation, oldest first.
     *
     * @return list<array<string, mixed>>
     */
    public function releaseApprovals(): array
    {
        $releases = array_values(array_filter($this->approvals, fn (array $approval): bool => ReleaseApproval::is($approval)
            && ($approval['conversation_id'] ?? null) === $this->conversationId));
        usort($releases, fn (array $a, array $b): int => strcmp((string) ($a['created_at'] ?? ''), (string) ($b['created_at'] ?? '')));

        return $releases;
    }

    /**
     * Fetch the job of each turn or summary still in progress (to show a budget wait and its live runs), and catch
     * up the live output of runs not yet shown.
     */
    protected function afterChanges(): void
    {
        $this->trim();
        $approvalId = $this->attentionApprovalId();
        $needs = NeedsYou::from($this->conversation()['attention'] ?? null);
        // An item that names its kind and offers its own options needs no separate approval read, unless it is a land,
        // whose repository, commit and flags come from the approval.
        // A release approval (install or rollback) shows its pin and outcome from the approval too; once held, its own
        // change events keep it current. A browser action shows its values and screenshot from the approval as well.
        $selfDescribed = $needs !== null && $needs['kind'] !== null && ! in_array($needs['kind'], ['land', \App\Client\BrowserApproval::Kind, ...ReleaseApproval::Kinds], true) && $needs['options'] !== [];
        $held = $approvalId !== null ? ($this->approvals[$approvalId] ?? null) : null;
        if ($approvalId !== null && ! $selfDescribed && ($held === null || (! ReleaseApproval::is($held) && ($held['status'] ?? null) !== 'needs_owner'))) {
            $this->loadApproval($approvalId);
        }
        foreach ([...$this->turns(), ...$this->summaries()] as $item) {
            $jobId = $item['job_id'] ?? null;
            if (! is_string($jobId) || $this->finished($item)) {
                continue;
            }
            $owner = isset($item['turn_id']) ? ['kind' => 'turn', 'id' => (string) $item['turn_id']] : ['kind' => 'summary', 'id' => (string) $item['summary_id']];
            $job = $this->held('job', $jobId);
            if ($job === null) {
                $job = $this->attempt(fn () => $this->director()->get('jobs/'.rawurlencode($jobId)));
                if (! is_array($job)) {
                    continue;
                }
                $this->keep('job', $jobId, $job);
            }
            foreach ((array) ($job['live_run_ids'] ?? []) as $runId) {
                if (is_string($runId)) {
                    $this->runOwners[$runId] ??= $owner;
                    if (! isset($this->output[$runId])) {
                        $this->catchUp($runId);
                    }
                }
            }
        }
    }

    protected function applyRunEvents(array $events): void
    {
        $byRun = [];
        foreach ($events as $event) {
            if (($event['conversation_id'] ?? null) !== $this->conversationId || ! is_string($event['run_id'] ?? null)) {
                continue;
            }
            $runId = $event['run_id'];
            if (is_string($event['turn_id'] ?? null)) {
                $this->runOwners[$runId] ??= ['kind' => 'turn', 'id' => $event['turn_id']];
            } elseif (is_string($event['summary_id'] ?? null)) {
                $this->runOwners[$runId] ??= ['kind' => 'summary', 'id' => $event['summary_id']];
            }
            $byRun[$runId][] = $event;
        }

        foreach ($byRun as $runId => $runEvents) {
            if (! isset($this->output[$runId])) {
                $this->catchUp($runId);
            }
            if (isset($this->output[$runId])) {
                $this->output[$runId] = LiveUpdates::appendOutput($this->output[$runId], $runEvents);
            }
        }
    }

    /**
     * Post a turn. It is queued behind the active turn (from any device); when nothing runs and the conversation is
     * not held, it starts at once. One key per message: kept while the answer may be lost, replaced once recorded.
     */
    public function post(): void
    {
        $this->validate(['message' => ['required', 'string', 'max:'.self::MaxMessage]]);

        $answer = $this->send('conversations/'.rawurlencode($this->conversationId).'/turns', ['message' => $this->message], 'postKey',
            __('The answer was lost: :why. Sending again is safe: it replays the same message.'));
        if ($answer === null) {
            return;
        }

        $this->message = '';
        $turn = (array) ($answer['turn'] ?? []);
        $this->notice = ($turn['state'] ?? null) === 'queued' ? __('Queued behind the current reply.') : '';
    }

    public function retryTurn(string $turnId): void
    {
        $turn = $this->held('turn', $turnId);
        abort_unless(is_array($turn) && ($turn['state'] ?? null) === 'error', 404);

        $message = (string) ($turn['message'] ?? '');
        abort_if($message === '', 404);

        $key = $this->retryKeys[$turnId] ??= DirectorClient::newKey();
        $lost = __('The answer was lost: :why. Sending again is safe: it replays the same message.');
        try {
            $answer = $this->director()->write(
                'conversations/'.rawurlencode($this->conversationId).'/turns',
                ['message' => $message],
                $key,
            );
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = str_replace(':why', $unavailable->getMessage(), $lost);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->retryKeys[$turnId] = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();
            $this->notice = '';

            return;
        }

        $this->problem = '';
        $this->retryKeys[$turnId] = DirectorClient::newKey();
        foreach (['conversation' => 'conversation_id', 'turn' => 'turn_id'] as $kind => $idField) {
            $record = $answer->body[$kind] ?? null;
            if (is_array($record) && is_string($record[$idField] ?? null)) {
                $this->keep($kind, $record[$idField], $record);
            }
        }
        $this->afterChanges();
        $turn = (array) ($answer->body['turn'] ?? []);
        $this->notice = ($turn['state'] ?? null) === 'queued'
            ? __('Queued behind the current reply.')
            : __('Sent the message again.');
    }

    public function retryTurnWithModel(string $turnId): void
    {
        $turn = $this->held('turn', $turnId);
        abort_unless(is_array($turn) && ($turn['state'] ?? null) === 'error', 404);

        abort_unless(TurnFailure::offersModelSwitch($turn), 404);

        $message = (string) ($turn['message'] ?? '');
        abort_if($message === '', 404);

        $conversation = $this->conversation();
        abort_unless(is_array($conversation), 404);

        $modelNames = array_column($this->modelChoices, 'name');
        $currentModel = is_string($conversation['model'] ?? null) ? $conversation['model'] : '';
        $model = $this->retryModel[$turnId] ?? $this->suggestedRetryModel($currentModel);
        if (! in_array($model, $modelNames, true)) {
            $this->addError('retryModel.'.$turnId, __('Pick an available model.'));

            return;
        }

        // The Director's built-in chat on the chosen model, on the same machine.
        $body = array_filter([
            'node' => $conversation['node'],
            'spec' => 'chat',
            'model' => $model,
            'intent_id' => filled($conversation['intent_id'] ?? null) ? (string) $conversation['intent_id'] : null,
            'project_id' => filled($conversation['intent_id'] ?? null) || ! filled($conversation['project_id'] ?? null)
                ? null
                : (string) $conversation['project_id'],
        ], fn ($value) => $value !== null);

        $continueKey = $this->continueKeys[$turnId] ??= DirectorClient::newKey();
        $postKey = DirectorClient::newKey();

        try {
            $answer = $this->director()->write('conversations', $body, $continueKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Starting again is safe; it replays the same request.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->continueKeys[$turnId] = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return;
        }

        $this->continueKeys[$turnId] = DirectorClient::newKey();
        $newId = (string) $answer->body['conversation']['conversation_id'];

        $label = ModelCatalog::labelFor($model, $this->modelChoices);
        // The new conversation exists either way: link it, so a second click does not start another.
        $this->continuedIn[$turnId] = [
            'conversation_id' => $newId,
            'model' => $model,
            'label' => $label,
        ];

        try {
            $this->director()->write(
                'conversations/'.rawurlencode($newId).'/turns',
                ['message' => $message],
                $postKey,
            );
        } catch (DirectorUnavailable|DirectorRefused $e) {
            $this->notice = '';
            $this->problem = __('A new conversation on :model started, but your message did not reach it: :why. Open it and send the message again.', [
                'model' => $label,
                'why' => $e instanceof DirectorRefused ? $e->explain() : $e->getMessage(),
            ]);

            return;
        }

        $this->notice = __('Continued in a new conversation on :model.', ['model' => $label]);
        $this->problem = '';
    }

    private function suggestedRetryModel(string $currentModel): string
    {
        foreach ($this->modelChoices as $choice) {
            if ($choice['name'] !== $currentModel) {
                return $choice['name'];
            }
        }

        return $this->modelChoices[0]['name'] ?? $currentModel;
    }

    /**
     * Stop: hold the queue at once and request cancel of the active turn's job. The cancel stays "stopping" until the
     * node reports an outcome.
     */
    public function stop(): void
    {
        $answer = $this->send('conversations/'.rawurlencode($this->conversationId).'/stop', [], 'stopKey',
            __('The answer was lost: :why. Stopping again is safe.'));
        if ($answer !== null) {
            $this->notice = is_array($answer['cancel'] ?? null)
                ? __('Stopped: the queue is held, and the job for the current message is asked to stop.')
                : __('Stopped: the queue is held.');
        }
    }

    public function resume(): void
    {
        if ($this->send('conversations/'.rawurlencode($this->conversationId).'/resume', [], 'resumeKey',
            __('The answer was lost: :why. Resuming again is safe.')) !== null) {
            $this->notice = __('Resumed: the next queued message starts.');
        }
    }

    /**
     * Archive: the conversation leaves the lists (it stays readable, under Archived) and takes no messages until it
     * is unarchived; a lead's wake-ups stop meanwhile.
     */
    public function archive(): void
    {
        abort_if($this->chiefName !== '', 403);
        if ($this->send('conversations/'.rawurlencode($this->conversationId).'/archive', [], 'archiveKey',
            __('The answer was lost: :why. Archiving again is safe.')) !== null) {
            $this->notice = __('Archived. It is listed under Archived, and takes messages again once unarchived.');
            $this->dispatch('threads-changed');
        }
    }

    public function unarchive(): void
    {
        if ($this->send('conversations/'.rawurlencode($this->conversationId).'/unarchive', [], 'archiveKey',
            __('The answer was lost: :why. Unarchiving again is safe.')) !== null) {
            $this->notice = __('Unarchived.');
            $this->dispatch('threads-changed');
        }
    }

    /**
     * Delete for good: its messages and summaries go; task lanes it started stay, no longer tied to it. The Director
     * refuses while a message is queued or running (`busy`), and refuses a lead's own credential.
     */
    public function deleteConversation(): void
    {
        abort_if($this->chiefName !== '', 403);
        try {
            $this->director()->remove('conversations/'.rawurlencode($this->conversationId), $this->deleteKey);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = __('The answer was lost: :why. Deleting again is safe.', ['why' => $unavailable->getMessage()]);

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->status === 404) {
                $this->leaveDeleted();

                return;
            }
            if ($refused->is('idempotency_conflict')) {
                $this->deleteKey = DirectorClient::newKey();
            }
            $this->problem = match (true) {
                $refused->is('busy') => __('A message is still queued or running. Stop the conversation, then delete it.'),
                $refused->is('chief_of_staff') => __('This is the chief of staff. Turn it off in Settings to delete it.'),
                default => $refused->explain(),
            };
            $this->notice = '';

            return;
        }

        $this->leaveDeleted();
    }

    private function leaveDeleted(): void
    {
        $title = $this->conversation() !== null ? ConversationOrder::name($this->conversation()) : __('The conversation');
        session()->flash('conversation-notice', __('Deleted “:title”.', ['title' => $title]));
        $this->dispatch('threads-changed');
        $this->redirectRoute('conversations.index', navigate: true);
    }

    /**
     * Make a summary's portion again with a new job. Once done it is the newest summary of that portion.
     */
    public function redo(string $summaryId): void
    {
        abort_unless($this->held('summary', $summaryId) !== null, 404);

        if ($this->send('conversations/'.rawurlencode($this->conversationId).'/summaries/'.rawurlencode($summaryId).'/redo', [], 'redoKey',
            __('The answer was lost: :why. Asking again is safe.')) !== null) {
            $this->notice = __('A new summary of the same messages is being made.');
        }
    }

    public function setBudget(): void
    {
        $this->takeConversation($this->attempt(fn () => $this->budget->put($this->director(), 'conversations/'.rawurlencode($this->conversationId).'/budget')));
    }

    public function clearBudget(): void
    {
        $this->takeConversation($this->attempt(fn () => $this->budget->clear($this->director(), 'conversations/'.rawurlencode($this->conversationId).'/budget')));
    }

    /**
     * The action approval the conversation's attention links, when an agent's request waits on the owner
     * (CLIENT-API.md "Action approvals"): it is answered with Approve or Deny, not a reply.
     */
    public function attentionApprovalId(): ?string
    {
        $attention = $this->conversation()['attention'] ?? null;

        return is_array($attention) && is_string($attention['action_approval_id'] ?? null) && $attention['action_approval_id'] !== ''
            ? $attention['action_approval_id']
            : null;
    }

    /**
     * Adopt this hosted conversation (owner or admin): `PATCH conversations/{id}` with this member as its sponsor.
     * The Director revokes the old lead credentials in the same step and resumes paused work (CLIENT-API.md §11).
     */
    public function adopt(): void
    {
        $memberId = app(Identity::class)->memberId();
        if ($memberId === null || ! RecordActions::allows($this->conversation(), 'adopt', fn (): bool => app(Identity::class)->can('manage_members'))) {
            return;
        }

        $this->takeConversation($this->attempt(fn () => $this->director()->put(
            'conversations/'.rawurlencode($this->conversationId),
            ['sponsor_member_id' => $memberId],
            DirectorClient::newKey(),
            'PATCH',
        )));
    }

    /**
     * Answer the needs-you question with one of its options, only when pressed (docs/design/hosted-needs-you-client.md).
     * 403 or 404: it is no longer this member's to answer. 409: someone answered first, or it changed; read it again.
     */
    public function answerChoice(string $choice): void
    {
        $needs = NeedsYou::from($this->conversation()['attention'] ?? null);
        if ($needs === null || ! in_array($choice, array_column($needs['options'], 'id'), true)) {
            return;
        }

        try {
            if ($needs['approval'] !== null) {
                $answer = $this->director()->write('action-approvals/'.rawurlencode($needs['approval']).'/answer', ['choice' => $choice], DirectorClient::newKey());
                if (is_string($answer->body['id'] ?? null)) {
                    $this->approvals[$answer->body['id']] = $answer->body;
                }
            } elseif ($needs['backlog'] !== null && $needs['project'] !== null) {
                $this->director()->write('projects/'.rawurlencode($needs['project']).'/backlog/'.rawurlencode($needs['backlog']).'/answer',
                    ['choice' => $choice, 'revision' => $needs['revision']], DirectorClient::newKey());
            } else {
                return;
            }
            $this->problem = '';
        } catch (DirectorRefused $refused) {
            $this->problem = match (true) {
                in_array($refused->status, [403, 404], true) => __('This question is no longer yours to answer here.'),
                $refused->status === 409 => __('This question changed or was answered elsewhere. It shows its latest state.'),
                default => $refused->explain(),
            };
            if ($refused->status === 401) {
                $this->refused($refused);

                return;
            }
            app(Identity::class)->forget();
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();

            return;
        }

        $problem = $this->problem;
        $this->refetch();
        $this->problem = $problem;
    }

    public function dismissAttention(): void
    {
        $this->takeConversation($this->attempt(fn () => $this->director()->replace(
            'conversations/'.rawurlencode($this->conversationId).'/attention',
            null,
        )));
    }

    /**
     * @return array<string, mixed>|null
     */
    public function conversation(): ?array
    {
        return $this->held('conversation', $this->conversationId);
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function turns(): array
    {
        $turns = $this->heldOf('turn');
        usort($turns, fn (array $a, array $b): int => ((int) $a['seq']) <=> ((int) $b['seq']));

        return $turns;
    }

    /**
     * @return list<array<string, mixed>>
     */
    public function summaries(): array
    {
        $summaries = $this->heldOf('summary');
        usort($summaries, fn (array $a, array $b): int => ((int) $a['through_seq']) <=> ((int) $b['through_seq']) ?: strcmp((string) $a['created_at'], (string) $b['created_at']));

        return $summaries;
    }

    /**
     * The summaries placed after a turn: those covering turns 1 to that turn's seq.
     *
     * @return list<array<string, mixed>>
     */
    public function summariesAfter(int $seq): array
    {
        return array_values(array_filter($this->summaries(), fn (array $summary): bool => (int) $summary['through_seq'] === $seq));
    }

    /**
     * Whether a later summary redoes this one.
     */
    public function redoneBy(string $summaryId): ?string
    {
        foreach ($this->summaries() as $summary) {
            if (($summary['redoes'] ?? null) === $summaryId) {
                return (string) $summary['summary_id'];
            }
        }

        return null;
    }

    /**
     * The partial output of a turn's or summary's runs while it is in progress.
     */
    public function partial(string $kind, string $id): string
    {
        $text = '';
        foreach ($this->runOwners as $runId => $owner) {
            if ($owner['kind'] === $kind && $owner['id'] === $id) {
                $text .= $this->output[$runId]['text'] ?? '';
            }
        }

        return $text;
    }

    /**
     * @param  array<string, mixed>  $item  a turn or summary
     * @return array<string, mixed>|null its job, when held
     */
    public function jobOf(array $item): ?array
    {
        return is_string($item['job_id'] ?? null) ? $this->held('job', $item['job_id']) : null;
    }


    /**
     * @param  array<string, mixed>  $item
     */
    public function finished(array $item): bool
    {
        return in_array($item['state'] ?? null, ['done', 'error', 'stopped'], true);
    }

    /**
     * A write under the write rules, its answer applied to the records held. The key named by $keyProperty is kept
     * while the answer may have been lost, and replaced once the write is recorded or turns out to name another.
     *
     * @param  array<string, mixed>  $body
     * @return array<string, mixed>|null
     */
    private function send(string $path, array $body, string $keyProperty, string $lost): ?array
    {
        try {
            $answer = $this->director()->write($path, $body, $this->{$keyProperty});
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = str_replace(':why', $unavailable->getMessage(), $lost);

            return null;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->{$keyProperty} = DirectorClient::newKey();
            }
            $this->problem = $refused->is('chief_of_staff')
                ? __('This is the chief of staff. Turn it off in Settings to archive it.')
                : $refused->explain();
            $this->notice = '';

            return null;
        }

        $this->problem = '';
        $this->{$keyProperty} = DirectorClient::newKey();
        foreach (['conversation' => 'conversation_id', 'turn' => 'turn_id', 'summary' => 'summary_id'] as $kind => $idField) {
            $record = $answer->body[$kind] ?? null;
            if (is_array($record) && is_string($record[$idField] ?? null)) {
                $this->keep($kind, $record[$idField], $record);
            }
        }
        $this->afterChanges();

        return $answer->body;
    }

    /**
     * @param  array<string, mixed>|null  $answer
     */
    private function takeConversation(?array $answer): void
    {
        if (is_array($answer['conversation'] ?? null)) {
            $this->keep('conversation', $this->conversationId, $answer['conversation']);
        }
    }

    /**
     * Hold a record unless a newer revision of it is already held (§8 step 4).
     *
     * @param  array<string, mixed>  $record
     */
    private function keep(string $kind, string $id, array $record): void
    {
        $this->records = LiveUpdates::applyChange($this->records, ['kind' => $kind, 'id' => $id,
            'revision' => (int) ($record['revision'] ?? 0), 'record' => $record]) ?? $this->records;
    }

    private function catchUp(string $runId): void
    {
        $caught = LiveUpdates::catchUp($this->output[$runId] ?? null, fn (int $after): mixed => $this->attempt(fn () => $this->director()->find('runs/'.rawurlencode($runId).'/events?after='.$after)));
        if ($caught !== null) {
            $this->output[$runId] = $caught;
        }
    }
};
?>

@php($conversation = $this->conversation())
@php($attention = is_array($conversation['attention'] ?? null) && filled($conversation['attention']['question'] ?? null) ? $conversation['attention'] : null)

{{--
    A chat app's thread: a slim header, the transcript in one reading column (only it scrolls), the composer pinned
    under it, and a side panel for what goes with the thread: its lanes when it is a lead, and its details.
--}}
<div class="flex h-full" data-full-bleed
     @if (\App\Client\TurnWaiting::anyInProgress($this->turns())) wire:poll.10s="refetch" @endif
     x-data="{
        side: false,
        tab: @js($isLead ? 'lanes' : 'details'),
        init() {
            let stored = null;
            try { stored = localStorage.getItem('cs.sidepanel') } catch (e) {}
            // Remembered open or closed only where it sits beside the thread; on a narrow screen it covers the thread,
            // so it starts closed there and is opened on demand.
            const wide = window.matchMedia('(min-width: 1024px)').matches;
            this.side = wide && (stored === null ? @js($isLead) : stored === 'open');
        },
        toggleSide(tab = null) {
            this.side = tab !== null && tab !== this.tab ? true : ! this.side;
            if (tab !== null) { this.tab = tab }
            if (window.matchMedia('(min-width: 1024px)').matches) {
                try { localStorage.setItem('cs.sidepanel', this.side ? 'open' : 'closed') } catch (e) {}
            }
        },
     }">
    <section class="relative flex min-w-0 flex-1 flex-col" data-test="conversation-page"
             x-data="directorStream(@js(route('stream', ['conversation' => $conversationId])))">
        <header class="flex h-12 shrink-0 items-center gap-2 px-3 md:px-4">
            @if ($conversation)
                @if ($chiefName !== '')
                    <flux:icon.sparkles variant="micro" class="size-4 shrink-0 text-amber-500" />
                    <h1 class="min-w-0 truncate text-[15px] font-medium" data-test="conversation-title">{{ $chiefName }}</h1>
                @else
                <h1 class="min-w-0 truncate text-[15px] font-medium" data-test="conversation-title">{{ filled($conversation['title'] ?? null) ? $conversation['title'] : __('Untitled conversation with :model', ['model' => $conversation['model'] ?? __('an agent')]) }}</h1>
                @endif
                @if ($isLead)
                    <flux:icon.wrench-screwdriver variant="micro" class="size-3.5 shrink-0 text-zinc-400" title="{{ __('Runs tasks') }}" />
                @endif
                @if (($conversation['state'] ?? 'idle') !== 'idle')
                    <x-client.conversation-state :conversation="$conversation" />
                @endif
            @endif
            <x-client.stream-dot class="shrink-0" />
            <div class="ms-auto flex items-center gap-1">
                @if ($conversation && $chiefName === '')
                    @allowed($conversation, 'archive', 'control_work')
                    <flux:dropdown position="bottom" align="end">
                        <flux:button size="sm" variant="ghost" square icon="ellipsis-horizontal" :aria-label="__('Conversation actions')" data-test="conversation-menu" />
                        <flux:menu>
                            @if (filled($conversation['archived_at'] ?? null))
                                <flux:menu.item icon="archive-box-arrow-down" wire:click="unarchive" data-test="unarchive">{{ __('Unarchive') }}</flux:menu.item>
                            @else
                                <flux:menu.item icon="archive-box" wire:click="archive" data-test="archive">{{ __('Archive') }}</flux:menu.item>
                            @endif
                            <flux:modal.trigger name="delete-conversation">
                                <flux:menu.item icon="trash" variant="danger" data-test="delete">{{ __('Delete…') }}</flux:menu.item>
                            </flux:modal.trigger>
                        </flux:menu>
                    </flux:dropdown>
                    @endallowed
                @endif
                <flux:tooltip :content="__('Thread panel')">
                    <flux:button size="sm" variant="ghost" square x-on:click="toggleSide()" :aria-label="__('Thread panel')" data-test="side-toggle">
                        <x-client.icon.panel-right />
                    </flux:button>
                </flux:tooltip>
            </div>
        </header>

        <div class="relative min-h-0 flex-1" x-data="{ stick: true }">
            <div class="h-full overflow-y-auto"
                 x-init="requestAnimationFrame(() => { $el.scrollTop = $el.scrollHeight }); new MutationObserver(() => { if (stick) { $el.scrollTop = $el.scrollHeight } }).observe($el, { childList: true, subtree: true, characterData: true })"
                 x-on:scroll.passive="stick = $el.scrollHeight - $el.scrollTop - $el.clientHeight < 80">
                <div class="mx-auto flex w-full max-w-3xl flex-col gap-3 px-4 pb-6 pt-4 md:px-6" data-test="thread">
                    @if ($problem !== '')
                        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
                            <flux:callout.text>{{ $problem }}</flux:callout.text>
                        </flux:callout>
                    @endif

                    @if ($conversation)
                        @php($turns = $this->turns())
                        @if ($turns === [])
                            <div class="py-24 text-center text-zinc-500" data-test="no-turns">{{ __('No messages yet. Say something below.') }}</div>
                        @endif

                        @if ($this->hasEarlier())
                            <div class="flex justify-center">
                                <flux:button size="sm" variant="ghost" wire:click="showEarlier" data-test="show-earlier">{{ __('Show earlier messages') }}</flux:button>
                            </div>
                        @endif

                        @foreach ($turns as $turn)
                            <article class="group flex flex-col gap-2" wire:key="turn-{{ $turn['turn_id'] }}"
                                     data-test="turn" data-turn="{{ $turn['turn_id'] }}" data-state="{{ $turn['state'] }}">
                                {{-- A wake turn is the agent's follow-up when its lanes ended: no message from the person, only the reply. --}}
                                @if (is_array($turn['peer'] ?? null))
                                    {{-- A brief from another conversation (CLIENT-API.md §11.6, Peer messages): data, never instructions or approvals. Plain text only. --}}
                                    <div class="space-y-1.5 rounded-xl border border-dashed border-zinc-400/50 px-4 py-3" data-test="peer-brief">
                                        <div class="flex flex-wrap items-center gap-x-1.5 gap-y-1 text-xs text-zinc-500">
                                            <flux:icon.arrows-right-left variant="micro" />
                                            <span>{{ __('Brief from another conversation') }}:</span>
                                            <span class="font-medium text-zinc-700 dark:text-zinc-300" data-test="peer-from">{{ (string) ($turn['peer']['from_title'] ?? '') !== '' ? $turn['peer']['from_title'] : ($turn['peer']['from_conversation'] ?? '') }}</span>
                                            <span class="font-mono">({{ $turn['peer']['from_conversation'] ?? '' }})</span>
                                            <flux:badge size="sm" color="amber" data-test="peer-untrusted">{{ __('Untrusted') }}</flux:badge>
                                        </div>
                                        <div class="whitespace-pre-wrap break-words text-sm" data-test="peer-text">{{ $turn['message'] ?? '' }}</div>
                                    </div>
                                @elseif (($releaseWake = ReleaseApproval::wakeApprovalId($turn)) !== null)
                                    {{-- The node's post-install wake (CLIENT-API.md "Hosted release approvals"): its summary, linked to the approval. --}}
                                    <div class="space-y-1 text-xs text-zinc-500" data-test="release-wake">
                                        <div class="flex flex-wrap items-center gap-x-1.5 gap-y-1">
                                            <flux:icon.rocket-launch variant="micro" />
                                            <span>{{ __('The node reported on release request') }}</span>
                                            <a href="#release-{{ $releaseWake }}" class="font-mono hover:text-zinc-800 hover:underline dark:hover:text-zinc-200" data-test="release-wake-approval">{{ $releaseWake }}</a>
                                        </div>
                                        <p class="whitespace-pre-wrap break-words" data-test="release-wake-text">{{ $turn['message'] }}</p>
                                    </div>
                                @elseif (($turn['origin'] ?? 'person') === 'wake')
                                    @php($wakeJobs = array_values(array_filter((array) (is_array($turn['detail'] ?? null) ? ($turn['detail']['jobs'] ?? []) : []), 'is_string')))
                                    <div class="flex flex-wrap items-center gap-x-1.5 gap-y-1 text-xs text-zinc-500" data-test="wake-marker">
                                        <flux:icon.check-circle variant="micro" class="text-emerald-500" />
                                        <span>{{ trans_choice('A task lane finished|Task lanes finished', max(1, count($wakeJobs))) }}</span>
                                        @foreach ($wakeJobs as $jobId)
                                            <span aria-hidden="true">·</span>
                                            <a href="{{ route('lanes.show', $jobId) }}" wire:navigate class="font-mono hover:text-zinc-800 hover:underline dark:hover:text-zinc-200" wire:key="wake-{{ $turn['turn_id'] }}-{{ $jobId }}">{{ $jobId }}</a>
                                        @endforeach
                                    </div>
                                @else
                                    <div class="flex justify-end">
                                        <x-client.markdown :text="$turn['message']" breaks class="max-w-[85%] rounded-3xl bg-zinc-100 px-4 py-2.5 !text-[15px] !leading-6 dark:bg-white/[.08] [&_.cs-code]:my-2 [&_.cs-code]:bg-white/60 dark:[&_.cs-code]:bg-black/20" data-test="turn-message" />
                                    </div>
                                @endif

                                @php($job = $this->jobOf($turn))
                                @if (($job['state'] ?? null) === 'waiting')
                                    <div class="flex items-center gap-2 text-sm text-amber-600 dark:text-amber-400" data-test="turn-waiting">
                                        <flux:icon.clock variant="micro" />
                                        <span>{{ $job['budget']['detail'] ?? __('Waiting for budget.') }}</span>
                                    </div>
                                @endif

                                <div class="min-w-0">
                                    @if ($turn['state'] === 'done')
                                        <x-client.markdown :text="$turn['reply'] ?? ''" class="!text-[15px] !leading-7" data-test="turn-reply" />
                                    @elseif ($turn['state'] === 'stopped')
                                        <p class="text-sm text-zinc-500" data-test="turn-detail">{{ $turn['detail'] ?? __('Stopped before it replied.') }}</p>
                                    @elseif ($turn['state'] === 'error')
                                        <div class="space-y-3 rounded-xl border border-red-500/25 bg-red-500/[.05] px-4 py-3">
                                            <p class="text-sm text-red-700 dark:text-red-300" data-test="turn-detail">{{ TurnFailure::plainReason($turn) }}</p>
                                            @if (($failureDetail = TurnFailure::detail($turn)) !== null)
                                                <p class="font-mono text-xs break-words text-red-700/80 dark:text-red-300/80" data-test="turn-failure-detail">{{ $failureDetail }}</p>
                                            @endif
                                            <div class="flex flex-wrap items-end gap-2" data-test="turn-retry">
                                                <flux:button size="sm" variant="primary" wire:click="retryTurn({{ Js::from($turn['turn_id']) }})" data-test="retry-turn">{{ __('Try again') }}</flux:button>
                                                @if (TurnFailure::offersModelSwitch($turn) && count($modelChoices) > 0)
                                                    <flux:select size="sm" wire:model="retryModel.{{ $turn['turn_id'] }}" :aria-label="__('Model to try')" class="min-w-44 max-w-60">
                                                        @foreach ($modelChoices as $choice)
                                                            <flux:select.option :value="$choice['name']" wire:key="retry-model-{{ $turn['turn_id'] }}-{{ $choice['name'] }}">
                                                                {{ $choice['label'] }}
                                                            </flux:select.option>
                                                        @endforeach
                                                    </flux:select>
                                                    <flux:button size="sm" wire:click="retryTurnWithModel({{ Js::from($turn['turn_id']) }})" data-test="retry-turn-model">{{ __('Try with another model') }}</flux:button>
                                                @endif
                                            </div>
                                            @if (isset($continuedIn[$turn['turn_id']]))
                                                <p class="text-sm" data-test="continued-conversation">
                                                    <flux:link :href="route('conversations.show', $continuedIn[$turn['turn_id']]['conversation_id'])" wire:navigate>
                                                        {{ __('Continued in a new conversation on :model', ['model' => $continuedIn[$turn['turn_id']]['label']]) }}
                                                    </flux:link>
                                                </p>
                                            @endif
                                        </div>
                                    @elseif (in_array($turn['state'], ['running', 'queued'], true) && \App\Client\TurnWaiting::words($turn) !== '')
                                        <p class="flex items-center gap-2 text-sm text-zinc-500" data-test="turn-waiting">
                                            <span class="size-1.5 rounded-full bg-amber-500"></span>
                                            {{ \App\Client\TurnWaiting::words($turn) }}
                                        </p>
                                    @elseif ($turn['state'] === 'running')
                                        @php($partial = $this->partial('turn', $turn['turn_id']))
                                        <x-client.markdown :text="$partial" :colour="false" class="!text-[15px] !leading-7" data-test="turn-partial" />
                                        <p class="mt-1 flex items-center gap-2 text-xs text-zinc-500">
                                            <span class="size-1.5 animate-pulse rounded-full bg-blue-500"></span>
                                            {{ $partial === '' ? __('Waiting for the reply…') : __('Writing… the finished reply replaces this.') }}
                                        </p>
                                    @elseif ($turn['state'] === 'summarising')
                                        <p class="text-sm text-zinc-500">{{ __('Waiting for a summary of older messages before it starts.') }}</p>
                                    @else
                                        <p class="text-sm text-zinc-500">{{ ($conversation['held'] ?? false) ? __('Queued; the conversation is stopped. Resume to start it.') : __('Queued behind the current reply.') }}</p>
                                    @endif
                                </div>

                                {{-- The turn's particulars: out of the way until you point at the turn. --}}
                                <div class="-mt-0.5 flex min-h-7 flex-wrap items-center gap-x-2 gap-y-1 text-xs text-zinc-500 transition-opacity md:pointer-fine:opacity-0 md:pointer-fine:group-hover:opacity-100 md:pointer-fine:focus-within:opacity-100">
                                    @if ($turn['state'] === 'done')
                                        <button type="button" x-data="{ copied: false }"
                                                x-on:click="if (await csCopy($el.closest('article').querySelector('[data-test=turn-reply]').innerText)) { copied = true; setTimeout(() => copied = false, 1500) }"
                                                class="-ms-1 flex size-7 items-center justify-center rounded-md hover:bg-black/5 hover:text-zinc-900 dark:hover:bg-white/[.07] dark:hover:text-white"
                                                aria-label="{{ __('Copy the reply') }}" title="{{ __('Copy the reply') }}">
                                            <flux:icon.document-duplicate variant="micro" x-show="! copied" />
                                            <flux:icon.check variant="micro" x-show="copied" x-cloak />
                                        </button>
                                    @endif
                                    @if (is_string($turn['run_id'] ?? null))
                                        <a href="{{ route('runs.show', $turn['run_id']) }}" wire:navigate class="hover:text-zinc-900 hover:underline dark:hover:text-white">{{ __('The run\'s evidence') }}</a>
                                        <span aria-hidden="true">·</span>
                                    @endif
                                    <span>#{{ $turn['seq'] }}</span>
                                    @if (filled($turn['posted_by_name'] ?? $turn['posted_by'] ?? null))
                                        <span>{{ $turn['posted_by_name'] ?? $turn['posted_by'] }}</span>
                                    @endif
                                    <x-client.time :at="$turn['posted_at']" :fallback="''" />
                                    @if ($turn['state'] !== 'done')
                                        <x-client.turn-state :turn="$turn" />
                                    @endif
                                    @if (is_array($turn['context'] ?? null))
                                        <span aria-hidden="true">·</span>
                                        <span data-test="turn-context">
                                            @if (is_string($turn['context']['summary_id'] ?? null))
                                                {{ __('Sent a summary of messages 1–:through, then', ['through' => $turn['context']['summary_through_seq']]) }}
                                            @else
                                                {{ __('Sent') }}
                                            @endif
                                            @if (($turn['context']['whole_from_seq'] ?? null) !== null && ($turn['context']['whole_through_seq'] ?? 0) >= ($turn['context']['whole_from_seq'] ?? 1))
                                                {{ __('messages :from–:through whole', ['from' => $turn['context']['whole_from_seq'], 'through' => $turn['context']['whole_through_seq']]) }}
                                            @else
                                                {{ __('no earlier messages') }}
                                            @endif
                                            ({{ number_format((int) ($turn['context']['tokens'] ?? 0)) }} {{ __('tokens') }})
                                            @if ($turn['context']['over_threshold'] ?? false)
                                                <span class="text-amber-600" data-test="over-threshold">{{ __('Over the threshold: no smaller form existed.') }}</span>
                                            @endif
                                        </span>
                                    @endif
                                </div>
                            </article>

                            @foreach ($this->summariesAfter((int) $turn['seq']) as $summary)
                                @php($redoneBy = $this->redoneBy($summary['summary_id']))
                                <aside class="rounded-xl border border-dashed border-black/15 px-3 py-2 dark:border-white/15"
                                       wire:key="summary-{{ $summary['summary_id'] }}" data-test="summary" data-summary="{{ $summary['summary_id'] }}" data-state="{{ $summary['state'] }}">
                                    <div class="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-zinc-500">
                                        <flux:icon.document-text variant="micro" />
                                        <span class="font-medium text-zinc-700 dark:text-zinc-300">{{ __('Summary of messages 1–:through', ['through' => $summary['through_seq']]) }}</span>
                                        <span>{{ $summary['state'] }}</span>
                                        @if (is_string($summary['redoes'] ?? null))
                                            <span>{{ __('redo of :id', ['id' => $summary['redoes']]) }}</span>
                                        @endif
                                        @if ($redoneBy !== null)
                                            <span data-test="summary-redone">{{ __('replaced by the redo :id', ['id' => $redoneBy]) }}</span>
                                        @endif
                                        @if (is_string($summary['run_id'] ?? null))
                                            <a href="{{ route('runs.show', $summary['run_id']) }}" wire:navigate class="hover:underline">{{ __('The run that made it') }}</a>
                                        @endif
                                        @if ($this->finished($summary))
                                            <flux:button size="xs" variant="ghost" icon="arrow-path" class="ms-auto" wire:click="redo({{ Js::from($summary['summary_id']) }})" data-test="redo-summary">{{ __('Regenerate summary') }}</flux:button>
                                        @endif
                                    </div>
                                    @if ($summary['state'] === 'done')
                                        <details class="mt-1">
                                            <summary class="cursor-pointer text-sm text-zinc-600 dark:text-zinc-300">{{ __('Read the summary') }}</summary>
                                            <div class="mt-2 whitespace-pre-wrap text-sm" data-test="summary-text">{{ $summary['text'] }}</div>
                                        </details>
                                    @elseif ($summary['state'] === 'error')
                                        <p class="mt-1 text-sm">{{ $summary['detail'] ?? __('The summary failed.') }}</p>
                                    @else
                                        <p class="mt-1 text-sm text-zinc-500">{{ __('Being made by an ordinary job…') }}</p>
                                        <div class="whitespace-pre-wrap text-sm" data-test="summary-partial">{{ $this->partial('summary', $summary['summary_id']) }}</div>
                                    @endif
                                </aside>
                            @endforeach
                        @endforeach

                        @php($attentionApproval = $this->attentionApprovalId() !== null ? ($this->approvals[$this->attentionApprovalId()] ?? null) : null)
                        @php($needsYou = \App\Client\NeedsYou::from($conversation['attention'] ?? null))
                        @php($releases = array_values(array_filter($this->releaseApprovals(), fn (array $approval): bool => $approval['id'] !== $this->attentionApprovalId())))
                        @if ($releases !== [] || $lastSmoke !== null)
                            <section class="space-y-3" data-test="release-approvals">
                                @foreach ($releases as $release)
                                    <x-client.release-approval :approval="$release" :show-conversation="false" />
                                @endforeach
                                @if ($lastSmoke !== null)
                                    <x-client.release-smoke :smoke="$lastSmoke" />
                                @endif
                            </section>
                        @endif
                        @if (ReleaseApproval::is($attentionApproval))
                            <x-client.release-approval :approval="$attentionApproval" :needs-you="true" :show-conversation="false" />
                        @elseif (\App\Client\BrowserApproval::is($attentionApproval))
                            <x-client.browser-approval :approval="$attentionApproval" :needs-you="true" />
                        @elseif ($needsYou !== null && $needsYou['options'] !== [])
                            <x-client.needs-you-choices :needs="$needsYou" :approval="$attentionApproval" />
                        @elseif (is_array($attentionApproval))
                            <x-client.action-approval :approval="$attentionApproval" />
                        @elseif ($attention !== null)
                            <article class="space-y-2 rounded-xl border border-amber-500/30 bg-amber-500/[.06] px-4 py-3" data-test="attention-message">
                                <div class="flex items-center gap-2 text-xs text-zinc-600 dark:text-zinc-400">
                                    <flux:icon.bell variant="micro" class="text-amber-500" />
                                    <span class="font-medium text-zinc-800 dark:text-zinc-200" data-test="attention-from">{{ \App\Client\NeedsYou::assignedElsewhere($attention) ?? __('Needs your answer') }}</span>
                                    @if (filled($attention['set_at'] ?? null))
                                        <span>· <x-client.time :at="$attention['set_at']" /></span>
                                    @endif
                                    <flux:button size="xs" variant="ghost" class="ms-auto" wire:click="dismissAttention" data-test="dismiss-attention">{{ __('Dismiss') }}</flux:button>
                                </div>
                                <x-client.markdown :text="$attention['question']" class="!text-[15px] !leading-7" data-test="needs-you-question" />
                                <p class="text-xs text-zinc-500">
                                    @if (is_string($attention['job_id'] ?? null))
                                        {{ __('About') }} <flux:link :href="route('lanes.show', $attention['job_id'])" wire:navigate class="font-mono" data-test="attention-job">{{ $attention['job_id'] }}</flux:link> ·
                                    @endif
                                    {{ __('Reply below to answer.') }}
                                </p>
                            </article>
                        @endif
                    @endif
                </div>
            </div>
            <button type="button" x-show="! stick" x-cloak x-transition.opacity
                    x-on:click="$el.previousElementSibling.scrollTo({ top: $el.previousElementSibling.scrollHeight, behavior: 'smooth' })"
                    class="absolute bottom-3 left-1/2 flex size-8 -translate-x-1/2 items-center justify-center rounded-full border border-black/10 bg-white text-zinc-600 shadow-sm hover:text-zinc-900 dark:border-white/10 dark:bg-cs-raised dark:text-zinc-300 dark:hover:text-white"
                    aria-label="{{ __('Jump to the latest') }}">
                <flux:icon.arrow-down variant="micro" />
            </button>
        </div>

        @if ($conversation)
            <div class="mx-auto w-full max-w-3xl shrink-0 px-4 pb-4 md:px-6">
                @if ($notice !== '')
                    <p class="mb-2 px-1 text-sm text-zinc-500" data-test="notice">{{ $notice }}</p>
                @endif
                @if (filled($paused = \App\Client\PausedReason::words($conversation)))
                    <p class="mb-2 px-1 text-sm text-amber-600 dark:text-amber-400" data-test="conversation-paused">{{ $paused }}</p>
                @endif
                @if (filled($conversation['archived_at'] ?? null))
                    {{-- An archived conversation takes no messages (the Director answers 409 archived): offer to unarchive instead. --}}
                    <div class="flex items-center gap-3 rounded-[1.5rem] border border-black/10 px-4 py-3 text-sm text-zinc-600 dark:border-white/10 dark:text-zinc-300" data-test="archived-banner">
                        <flux:icon.archive-box variant="micro" class="shrink-0 text-zinc-400" />
                        <span class="flex-1">{{ __('This conversation is archived. Unarchive it to send messages.') }}</span>
                        @allowed($conversation, 'archive', 'control_work')
                        <flux:button size="sm" wire:click="unarchive" data-test="archived-unarchive">{{ __('Unarchive') }}</flux:button>
                        @endallowed
                    </div>
                @else
                @capable('start_work')
                @error('message')
                    <p class="mb-2 px-1 text-sm text-red-600 dark:text-red-400" role="alert" data-test="message-error">{{ $message }}</p>
                @enderror
                <div data-test="composer" data-cs-composer x-data="composer" x-on:click="focusBox($event)"
                     class="rounded-[1.5rem] border border-black/10 bg-white px-3 pb-2 pt-3 shadow-[0_1px_2px_rgba(0,0,0,.04),0_6px_20px_rgba(0,0,0,.05)] transition-colors focus-within:border-black/20 dark:border-white/10 dark:bg-cs-raised dark:shadow-none dark:focus-within:border-white/20">
                    {{-- Flux's rich-text editor: Markdown shortcuts as you type, Markdown when sent (resources/js/composer.js). --}}
                    <div wire:ignore>
                        <ui-editor x-ref="editor" class="block w-full" data-flux-editor data-flux-control aria-label="{{ __('Message') }}"
                                   placeholder="{{ $attention !== null ? __('Answer the question above…') : ($isLead ? __('What should we work on?') : __('Message…')) }}"
                                   x-on:keydown.capture="keydown($event)">
                            <ui-editor-content></ui-editor-content>
                        </ui-editor>
                    </div>
                    <div class="mt-2 flex items-center gap-1.5">
                        @if (filled($conversation['model'] ?? null))
                            <span class="truncate px-1.5 text-xs text-zinc-500" title="{{ __('Model') }}">{{ $conversation['model'] }}</span>
                        @endif
                        @if (count((array) ($conversation['queued_turn_ids'] ?? [])) > 0)
                            <span class="text-xs text-zinc-500" data-test="queued">{{ trans_choice(':count message queued|:count messages queued', count($conversation['queued_turn_ids'])) }}</span>
                        @endif
                        <div class="ms-auto flex items-center gap-1.5">
                            @if (! \App\Client\RecordActions::allowsOr($conversation, 'control', 'control_work'))
                            @elseif ($conversation['held'] ?? false)
                                <flux:button type="button" size="sm" variant="ghost" icon="play" wire:click="resume" data-test="resume">{{ __('Resume') }}</flux:button>
                            @elseif (filled($conversation['active_turn_id'] ?? null) || count((array) ($conversation['queued_turn_ids'] ?? [])) > 0)
                                <flux:tooltip :content="__('Stop')">
                                    <button type="button" wire:click="stop" data-test="stop" aria-label="{{ __('Stop') }}"
                                            class="flex size-8 items-center justify-center rounded-full border border-black/15 text-zinc-700 hover:bg-black/5 dark:border-white/15 dark:text-zinc-200 dark:hover:bg-white/[.07]">
                                        <flux:icon.stop variant="micro" />
                                    </button>
                                </flux:tooltip>
                            @endif
                            <button type="button" x-on:click="send()" x-bind:disabled="empty || sending" aria-label="{{ __('Send') }}" title="{{ __('Send') }}" data-test="send"
                                    class="flex size-8 items-center justify-center rounded-full bg-zinc-900 text-white transition-opacity hover:opacity-85 disabled:opacity-30 dark:bg-white dark:text-zinc-900">
                                <flux:icon.arrow-up variant="micro" />
                            </button>
                        </div>
                    </div>
                </div>
                @else
                    <p class="px-1 text-sm text-zinc-500" data-test="read-only">{{ __('Your role can read this conversation but not send messages to it.') }}</p>
                @endcapable
                @endif
            </div>
        @endif
    </section>

    <flux:modal name="delete-conversation" class="max-w-md" data-test="delete-modal">
        <div class="space-y-4">
            <flux:heading size="lg">{{ __('Delete this conversation?') }}</flux:heading>
            <flux:text>{{ __('Its messages and summaries are deleted for good. Task lanes it started stay, no longer tied to it. To keep it out of the way instead, archive it.') }}</flux:text>
            <div class="flex justify-end gap-2">
                <flux:modal.close>
                    <flux:button variant="ghost">{{ __('Cancel') }}</flux:button>
                </flux:modal.close>
                <flux:button variant="danger" wire:click="deleteConversation" data-test="confirm-delete">{{ __('Delete') }}</flux:button>
            </div>
        </div>
    </flux:modal>

    {{-- The side panel: what goes with this thread. Docked on wide screens, over the thread on narrow ones. --}}
    <aside x-show="side" x-cloak x-on:keydown.escape.window="if (side && ! window.matchMedia('(min-width: 1024px)').matches) { side = false }"
           class="flex w-[22rem] shrink-0 flex-col border-s border-black/[.06] bg-white dark:border-white/[.06] dark:bg-cs-main max-lg:fixed max-lg:inset-y-0 max-lg:right-0 max-lg:z-30 max-lg:w-[min(22rem,100%)] max-lg:shadow-2xl"
           data-test="side-panel">
        <div class="flex h-12 shrink-0 items-center gap-1 px-2">
            @if ($isLead)
                <button type="button" x-bind:aria-pressed="tab === 'lanes'" x-on:click="tab = 'lanes'" x-bind:class="tab === 'lanes' ? 'bg-black/[.06] text-zinc-900 dark:bg-white/10 dark:text-white' : 'text-zinc-500 hover:text-zinc-900 dark:hover:text-white'"
                        class="rounded-lg px-2.5 py-1 text-sm">{{ __('Lanes') }}</button>
            @endif
            <button type="button" x-bind:aria-pressed="tab === 'details'" x-on:click="tab = 'details'" x-bind:class="tab === 'details' ? 'bg-black/[.06] text-zinc-900 dark:bg-white/10 dark:text-white' : 'text-zinc-500 hover:text-zinc-900 dark:hover:text-white'"
                    class="rounded-lg px-2.5 py-1 text-sm">{{ __('Details') }}</button>
            <flux:button size="sm" variant="ghost" square icon="x-mark" class="ms-auto" x-on:click="toggleSide()" :aria-label="__('Close the panel')" />
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto px-4 pb-4">
            @if ($isLead)
                <div x-show="tab === 'lanes'" data-test="lanes-pane">
                    <livewire:client.thread-lanes :conversation-id="$conversationId" />
                </div>
            @endif

            <div x-show="tab === 'details'" class="space-y-6" data-test="conversation-details">
                @if ($conversation)
                    <dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
                        <dt class="text-zinc-500">{{ __('Spec') }}</dt>
                        <dd class="truncate font-mono text-xs leading-5">{{ $conversation['spec'] }}</dd>
                        @if (filled($conversation['model'] ?? null))
                            <dt class="text-zinc-500">{{ __('Model') }}</dt>
                            <dd class="truncate font-mono text-xs leading-5">{{ $conversation['model'] }}</dd>
                        @endif
                        <dt class="text-zinc-500">{{ __('Machine') }}</dt>
                        <dd class="truncate font-mono text-xs leading-5">{{ $conversation['node'] }}</dd>
                        @if (is_string($conversation['intent_id'] ?? null))
                            <dt class="text-zinc-500">{{ __('Task') }}</dt>
                            <dd class="truncate"><flux:link :href="route('intents.show', $conversation['intent_id'])" wire:navigate class="font-mono text-xs">{{ $conversation['intent_id'] }}</flux:link></dd>
                        @endif
                        @if (is_string($conversation['project_id'] ?? null))
                            <dt class="text-zinc-500">{{ __('Project') }}</dt>
                            <dd class="truncate"><flux:link :href="route('projects.show', $conversation['project_id'])" wire:navigate class="font-mono text-xs">{{ $conversation['project_id'] }}</flux:link></dd>
                        @endif
                        @php($names = app(MemberNames::class))
                        @if (($sponsor = $names->label($conversation['sponsor_member_id'] ?? null, $conversation['sponsor_display_name'] ?? null)) !== null)
                            <dt class="text-zinc-500">{{ __('Sponsor') }}</dt>
                            <dd class="truncate" data-test="conversation-sponsor">{{ $sponsor }}</dd>
                        @endif
                        @if (($responsible = $names->label($conversation['responsible_member_id'] ?? null, $conversation['responsible_display_name'] ?? null)) !== null)
                            <dt class="text-zinc-500">{{ __('Responsible') }}</dt>
                            <dd class="truncate" data-test="conversation-responsible">{{ $responsible }}</dd>
                        @endif
                        <dt class="text-zinc-500">{{ __('Messages') }}</dt>
                        <dd>{{ $conversation['turns'] ?? 0 }}</dd>
                        <dt class="text-zinc-500">{{ __('ID') }}</dt>
                        <dd class="truncate font-mono text-xs leading-5" title="{{ $conversation['conversation_id'] }}">{{ $conversation['conversation_id'] }}</dd>
                    </dl>

                    @if ($isLead && \App\Client\RecordActions::allows($conversation, 'adopt', fn (): bool => is_array(app(Identity::class)->me()['capabilities'] ?? null)
                        && app(Identity::class)->can('manage_members') && ! app(Identity::class)->isMe($conversation['sponsor_member_id'] ?? null)))
                        <div class="space-y-2" data-test="adopt">
                            <p class="text-xs leading-5 text-zinc-500">{{ __('Adopting makes you this conversation\'s sponsor. Its agent\'s current credentials are revoked at once, and paused work resumes under you.') }}</p>
                            <flux:button size="sm" wire:click="adopt" wire:confirm="{{ __('Adopt this conversation? Its agent\'s current credentials stop working now.') }}">{{ __('Adopt') }}</flux:button>
                        </div>
                    @endif

                    @if ($peers !== [] && ! filled($conversation['archived_at'] ?? null))
                        <form wire:submit="sendPeer" class="space-y-2" data-test="peer-send">
                            <h2 class="text-sm font-medium">{{ __('Send a brief to another conversation') }}</h2>
                            <p class="text-xs leading-5 text-zinc-500">{{ __('It arrives there as untrusted text: information, never instructions or approvals.') }}</p>
                            <flux:select size="sm" wire:model="peerTo" :placeholder="__('Choose…')" :aria-label="__('To')">
                                @foreach ($peers as $peer)
                                    <flux:select.option :value="$peer" wire:key="peer-{{ $peer }}">{{ $peer }}</flux:select.option>
                                @endforeach
                            </flux:select>
                            <flux:textarea size="sm" wire:model="peerText" rows="3" :aria-label="__('Brief')" />
                            <flux:button size="sm" type="submit">{{ __('Send brief') }}</flux:button>
                            @if ($peerProblem !== '')
                                <p class="text-xs text-red-600 dark:text-red-400" data-test="peer-problem">{{ $peerProblem }}</p>
                            @endif
                        </form>
                    @endif

                    @if (is_array($conversation['compaction'] ?? null))
                        <p class="text-xs leading-5 text-zinc-500" data-test="compaction">
                            {{ __('Context window :window tokens (:source); messages send the whole thread up to :threshold tokens, then a summary plus the recent messages.', [
                                'window' => number_format((int) $conversation['compaction']['context_window']),
                                'source' => $conversation['compaction']['window_source'] ?? '',
                                'threshold' => number_format((int) $conversation['compaction']['threshold_tokens']),
                            ]) }}
                        </p>
                    @endif

                    <div class="space-y-3" data-test="conversation-budget">
                        <h2 class="text-sm font-medium">{{ __('Budget') }}</h2>
                        <p class="text-xs leading-5 text-zinc-500">{{ __('Dollars of actual pay-as-you-go spending, on top of its project\'s budget and the gateway\'s source budgets.') }}</p>
                        <x-client.budget :budget="$conversation['budget'] ?? null" />
                        @capable('manage_director_budgets')
                        <form wire:submit="setBudget" class="flex flex-wrap items-end gap-2">
                            <flux:input size="sm" wire:model="budget.usd" :label="__('Dollars')" type="number" step="0.01" min="0" class="w-24" />
                            <flux:select size="sm" wire:model="budget.period" :label="__('Per')" class="w-28">
                                <flux:select.option value="total">{{ __('total') }}</flux:select.option>
                                <flux:select.option value="day">{{ __('day') }}</flux:select.option>
                                <flux:select.option value="month">{{ __('month') }}</flux:select.option>
                            </flux:select>
                            <flux:button size="sm" type="submit">{{ __('Set') }}</flux:button>
                            @if (($conversation['budget']['state'] ?? 'none') !== 'none')
                                <flux:button size="sm" type="button" variant="ghost" wire:click="clearBudget" data-test="clear-budget">{{ __('Clear budget') }}</flux:button>
                            @endif
                        </form>
                        @endcapable
                    </div>
                @endif
            </div>
        </div>
    </aside>
@assets
    <flux:editor.scripts />
    <flux:editor.styles />
@endassets
</div>
