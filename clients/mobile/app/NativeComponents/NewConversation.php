<?php

namespace App\NativeComponents;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\DirectorScreen;
use App\Native\ModelPicker;
use Illuminate\View\View;

/**
 * Start a conversation (CLIENT-API.md §11.1 built-in chat, §11.1a hosted lead) and send its first message. A chat
 * talks to any catalog model; a lead runs on a model the catalog marks `lead` and can start and watch task lanes.
 * The model is chosen in two steps, provider then model (ModelPicker). Each request keeps one
 * Idempotency-Key until the Director has recorded it, so trying again after a lost answer creates nothing twice.
 */
class NewConversation extends DirectorScreen
{
    public const Chat = 'Chat with a model';

    public const Lead = 'Lead: can run task lanes';

    public string $kind = self::Chat;

    public string $title = '';

    public string $provider = '';

    public string $model = '';

    /** The harness that runs the model, when more than one can ('' when the catalog names none). */
    public string $harness = '';

    public string $message = '';

    /** @var array<string, mixed> GET models, kept so the picker can be rebuilt */
    public array $catalog = [];

    public string $createKey = '';

    public string $postKey = '';

    /** The conversation this screen created, once the Director has recorded it. */
    public string $conversationId = '';

    public function mount(): void
    {
        $this->createKey = DirectorClient::newKey();
        $this->postKey = DirectorClient::newKey();
        $this->attempt(function (DirectorClient $director): void {
            $this->catalog = $director->get('models');
        });
        [$this->provider, $this->model, $this->harness] = $this->picker()->initial();
    }

    public function updatedKind(): void
    {
        [$this->provider, $this->model, $this->harness] = $this->picker()->initial();
        $this->forgetCreated();
    }

    public function updatedProvider(): void
    {
        $this->model = $this->picker()->firstModel($this->provider);
        $this->harness = $this->picker()->firstHarness($this->provider, $this->model);
        $this->forgetCreated();
    }

    public function updatedModel(): void
    {
        $this->harness = $this->picker()->firstHarness($this->provider, $this->model);
        $this->forgetCreated();
    }

    public function updatedHarness(): void
    {
        $this->forgetCreated();
    }

    /**
     * @return list<string>
     */
    public function harnessChoices(): array
    {
        return $this->picker()->harnessChoices($this->provider, $this->model);
    }

    private function chosen(): string
    {
        return $this->picker()->value($this->provider, $this->model, $this->harness);
    }

    public function updatedTitle(): void
    {
        $this->forgetCreated();
    }

    /**
     * A change to what the conversation is means a new one: forget one already created for the old choice (its
     * first message did not arrive) and start the requests on fresh keys.
     */
    private function forgetCreated(): void
    {
        $this->conversationId = '';
        $this->createKey = DirectorClient::newKey();
        $this->postKey = DirectorClient::newKey();
    }

    public function picker(): ModelPicker
    {
        return ModelPicker::fromCatalog($this->catalog, leadsOnly: $this->isLead());
    }

    /**
     * @return list<string>
     */
    public function providerChoices(): array
    {
        return $this->picker()->providerChoices();
    }

    public function isLead(): bool
    {
        return $this->kind === self::Lead;
    }

    /**
     * @return list<string>
     */
    public function kindChoices(): array
    {
        return [self::Chat, self::Lead];
    }

    /**
     * @return list<string>
     */
    public function modelChoices(): array
    {
        return $this->picker()->modelChoices($this->provider);
    }

    public function where(): string
    {
        return $this->picker()->where($this->chosen());
    }

    public function start(): void
    {
        $model = $this->chosen();
        $message = trim($this->message);
        if ($model === '') {
            $this->problem = 'Choose a model.';

            return;
        }
        if ($message === '') {
            $this->problem = 'Write the first message.';

            return;
        }

        if ($this->conversationId === '' && ! $this->create($model)) {
            return;
        }

        try {
            $this->director()->write('conversations/'.rawurlencode($this->conversationId).'/turns', ['message' => $message], $this->postKey);
        } catch (DirectorUnavailable) {
            $this->problem = 'The conversation started, but the Director could not be reached for the first message. Sending again is safe.';

            return;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->postKey = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return;
        }

        $this->replace('/conversations/'.rawurlencode($this->conversationId));
    }

    public function render(): View
    {
        return view('native.new-conversation');
    }

    private function create(string $model): bool
    {
        $body = array_filter([
            'spec' => $this->isLead() ? 'hosted-lead' : 'chat',
            'model' => $model,
            'title' => trim($this->title) !== '' ? trim($this->title) : null,
        ], fn (?string $value): bool => $value !== null);

        try {
            $answer = $this->director()->write('conversations', $body, $this->createKey);
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the conversation may not have started. Starting again is safe.';

            return false;
        } catch (DirectorRefused $refused) {
            if ($refused->is('idempotency_conflict')) {
                $this->createKey = DirectorClient::newKey();
            }
            $this->problem = $refused->explain();

            return false;
        }

        $this->conversationId = Json::string(Json::record($answer->body['conversation'] ?? null)['conversation_id'] ?? null);
        if ($this->conversationId === '') {
            $this->problem = 'The Director did not say which conversation it started.';

            return false;
        }

        return true;
    }
}
