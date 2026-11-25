<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Json;
use App\Native\DirectorScreen;
use Illuminate\View\View;
use Native\Mobile\Attributes\Poll;

/**
 * The Director's conversations, newest first (CLIENT-API.md §11); each opens its thread. A hosted lead is marked.
 */
class Conversations extends DirectorScreen
{
    public const Shown = 100;

    /** @var list<array{id: string, title: string, lead: bool, model: string, state: string, turns: int}> */
    public array $conversations = [];

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
     * There is no event stream on the phone yet, so the open screen checks again every 20 seconds.
     */
    #[Poll(20000)]
    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $this->conversations = [];
            foreach (Json::records($director->get('conversations', ['limit' => self::Shown])['conversations'] ?? null) as $conversation) {
                $id = Json::string($conversation['conversation_id'] ?? null);
                if ($id === '') {
                    continue;
                }
                $turns = $conversation['turns'] ?? 0;
                $this->conversations[] = [
                    'id' => $id,
                    'title' => Json::string($conversation['title'] ?? null),
                    'lead' => ($conversation['lead'] ?? false) === true,
                    'model' => Json::string($conversation['model'] ?? null),
                    'state' => Json::string($conversation['state'] ?? null),
                    'turns' => is_int($turns) ? $turns : 0,
                ];
            }
            $this->loaded = true;
        });
    }

    public function newConversation(): void
    {
        $this->navigate('/conversations/new');
    }

    /**
     * @param  array{id: string, title: string, lead: bool, model: string, state: string, turns: int}  $conversation
     */
    public static function title(array $conversation): string
    {
        if ($conversation['title'] !== '') {
            return $conversation['title'];
        }

        return $conversation['lead'] ? 'Lead' : 'Conversation';
    }

    /**
     * @param  array{id: string, title: string, lead: bool, model: string, state: string, turns: int}  $conversation
     */
    public static function summary(array $conversation): string
    {
        $parts = array_filter([
            $conversation['lead'] ? 'Lead' : '',
            $conversation['model'],
            self::stateWords($conversation['state']),
            $conversation['turns'] === 1 ? '1 turn' : $conversation['turns'].' turns',
        ]);

        return implode(' · ', $parts);
    }

    public static function stateWords(string $state): string
    {
        return match ($state) {
            'running' => 'replying',
            'summarising' => 'summarising',
            'held' => 'stopped',
            default => '',
        };
    }

    public function render(): View
    {
        return view('native.conversations');
    }
}
