<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\DirectorScreen;
use App\Native\When;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

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
    public const MaxMessage = 200000;

    public string $conversationId = '';

    /** @var array<string, mixed> */
    public array $conversation = [];

    /** @var list<array<string, mixed>> newest first */
    public array $turns = [];

    public string $message = '';

    public string $notice = '';

    public string $postKey = '';

    public bool $loaded = false;

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
        $this->loaded = true;
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
            'held' => 'Held: '.Json::string($waiting['detail'] ?? null, 'the Director is holding this job').'.',
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
