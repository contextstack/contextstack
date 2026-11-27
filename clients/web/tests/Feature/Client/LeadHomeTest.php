<?php

use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * @return array<string, mixed>
 */
function homeConversation(array $over = []): array
{
    return [...['conversation_id' => 'con_chat', 'title' => 'planning chat', 'node' => 'workstation', 'model' => 'claude-code-sonnet', 'state' => 'idle',
        'attention' => null, 'queued_turn_ids' => [], 'turns' => 2, 'created_at' => '2026-11-13T10:00:00Z', 'revision' => 3], ...$over];
}

it('opens on what needs you while the chief of staff is off, and hides it from the sidebar', function () {
    Http::fake([director('conversations?*') => Http::response(['conversations' => [
        homeConversation(['conversation_id' => 'con_lead', 'lead' => true, 'created_at' => '2026-11-14T02:00:00Z']),
        homeConversation(['conversation_id' => 'con_asks', 'lead' => false, 'created_at' => '2026-11-12T00:00:00Z',
            'attention' => ['question' => 'Which branch?', 'set_at' => '2026-11-14T03:00:00Z']]),
    ]])]);
    fakeDirectorBackground();

    $this->get(route('home'))->assertRedirect(route('conversations.show', 'con_asks'));
    Http::assertSent(fn ($request) => $request->url() === director('conversations?limit=100'));
    $this->get(route('connect'))->assertDontSee('data-test="nav-chief-of-staff"', false);
});

it('opens a new conversation when there are none', function () {
    Http::fake([director('conversations?*') => Http::response(['conversations' => []])]);
    fakeDirectorBackground();

    $this->get(route('home'))->assertRedirect(route('conversations.create'));
});

it('marks the lead on the conversation list and puts it first', function () {
    Http::fake([
        director('conversations?*') => Http::response(['conversations' => [
            homeConversation(['conversation_id' => 'con_new', 'title' => 'newer chat', 'created_at' => '2026-11-13T12:00:00Z']),
            homeConversation(['conversation_id' => 'con_lead', 'title' => 'Hosted lead', 'lead' => true, 'created_at' => '2026-11-12T00:51:25Z']),
        ]]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.conversations')
        ->assertSee('data-test="lead-badge"', false)
        ->assertSeeTextInOrder(['Hosted lead', 'newer chat']);
});

it('lists conversations under Recents: those waiting on you first, then those that run tasks, then the newest', function () {
    Http::fake([
        director('conversations?*') => Http::response(['conversations' => [
            homeConversation(['conversation_id' => 'con_new', 'title' => 'newer chat', 'created_at' => '2026-11-13T12:00:00Z', 'state' => 'running']),
            homeConversation(['conversation_id' => 'con_lead', 'title' => 'Hosted lead', 'lead' => true, 'created_at' => '2026-11-12T00:51:25Z']),
            homeConversation(['conversation_id' => 'con_ask', 'title' => 'asks you', 'created_at' => '2026-11-01T00:00:00Z',
                'attention' => ['question' => 'Which repo?', 'set_at' => '2026-11-13T12:00:00Z', 'set_by' => 'cred_lead']]),
        ]]),
    ]);
    fakeDirectorBackground();

    $rail = Livewire::withoutLazyLoading()->test('client.thread-rail')
        ->assertSeeTextInOrder(['New chat', 'Recents', 'asks you', 'Hosted lead', 'newer chat'])
        ->assertDontSeeText('Bots')
        ->assertSee('data-test="rail-lead"', false)
        ->assertSee('data-test="rail-needs-you"', false)
        ->assertSee('data-test="rail-running"', false);
    expect(array_column($rail->get('threads'), 'id'))->toBe(['con_ask', 'con_lead', 'con_new']);
});
it('groups chats by project under Projects, named by the Director, a few at a time', function () {
    $chats = array_map(fn (int $n): array => homeConversation(['conversation_id' => "con_p{$n}", 'title' => "site chat {$n}", 'project_id' => 'prj_site',
        'created_at' => sprintf('2026-11-%02dT00:00:00Z', 10 + $n)]), range(1, 6));
    Http::fake([
        director('conversations?*') => Http::response(['conversations' => [...$chats, homeConversation(['conversation_id' => 'con_loose', 'title' => 'loose chat'])]]),
        director('projects') => Http::response(['projects' => [['project_id' => 'prj_site', 'name' => 'site rebuild']]]),
    ]);
    fakeDirectorBackground();

    $rail = Livewire::withoutLazyLoading()->test('client.thread-rail')
        ->assertSeeTextInOrder(['Projects', 'site rebuild', 'site chat 6', 'Recents', 'loose chat'])
        ->assertSee('data-test="rail-show-more"', false);
    expect($rail->get('projects'))->toHaveCount(1)
        ->and($rail->get('projects')[0]['threads'])->toHaveCount(6)
        ->and(array_column($rail->get('threads'), 'id'))->toBe(['con_loose']);
});

/**
 * @return array{intents: list<array<string, mixed>>, jobs: list<array<string, mixed>>}
 */
function threadLanesAnswer(): array
{
    return [
        'intents' => [
            ['intent_id' => 'int_mine', 'title' => 'Fix the widget', 'lead_conversation_id' => 'con_lead', 'meta' => ['repo' => 'contextstack', 'lane' => 'fix-widget'], 'created_at' => '2026-11-13T10:00:00Z', 'revision' => 1],
            ['intent_id' => 'int_other', 'title' => 'Someone else\'s lane', 'lead_conversation_id' => null, 'meta' => ['repo' => 'contextstack', 'lane' => 'other'], 'created_at' => '2026-11-13T10:00:00Z', 'revision' => 1],
        ],
        'jobs' => [
            ['job_id' => 'job_mine', 'intent_id' => 'int_mine', 'state' => 'done', 'created_at' => '2026-11-13T10:00:05Z', 'cancel' => null, 'revision' => 4,
                'land' => ['state' => 'landed', 'commit' => 'abc123def456', 'reason' => null]],
            ['job_id' => 'job_other', 'intent_id' => 'int_other', 'state' => 'accepted', 'created_at' => '2026-11-13T10:00:05Z', 'cancel' => null, 'revision' => 4],
        ],
    ];
}

it('shows only the lanes this lead started, with their state and land state', function () {
    Http::fake([director('intents*') => Http::response(threadLanesAnswer())]);
    fakeDirectorBackground();

    Livewire::test('client.thread-lanes', ['conversationId' => 'con_lead'])
        ->assertSeeText('Fix the widget')
        ->assertSeeText('contextstack/fix-widget')
        ->assertSeeText('landed')
        ->assertDontSeeText('Someone else');

    Http::assertSent(fn ($request) => str_contains($request->url(), 'intents?conversation=con_lead&limit=100'));
});

it('shows a lane the lead retried once, as its newest attempt with the earlier one\'s state', function () {
    $answer = threadLanesAnswer();
    $answer['jobs'][0] = [...$answer['jobs'][0], 'state' => 'error', 'land' => null];
    $answer['jobs'][] = ['job_id' => 'job_mine_retry', 'intent_id' => 'int_mine', 'state' => 'done', 'created_at' => '2026-11-13T11:00:00Z', 'cancel' => null, 'revision' => 7];
    Http::fake([director('intents*') => Http::response($answer)]);
    fakeDirectorBackground();

    $pane = Livewire::test('client.thread-lanes', ['conversationId' => 'con_lead']);

    expect($pane->get('lanes'))->toHaveCount(1)
        ->and($pane->get('lanes')[0]['job']['job_id'])->toBe('job_mine_retry');
    $pane->assertSee('data-job="job_mine_retry"', false)
        ->assertDontSee('data-job="job_mine"', false)
        ->assertSeeText('Attempt 2 · earlier: error');
});

it('shows no lanes from a Director that does not yet link lanes to conversations', function () {
    $answer = threadLanesAnswer();
    $answer['intents'] = array_map(function (array $intent): array {
        unset($intent['lead_conversation_id']);

        return $intent;
    }, $answer['intents']);
    Http::fake([director('intents*') => Http::response($answer)]);
    fakeDirectorBackground();

    Livewire::test('client.thread-lanes', ['conversationId' => 'con_lead'])
        ->assertSee('data-test="no-thread-lanes"', false)
        ->assertDontSeeText('Fix the widget');
});

it('shows the lanes pane on a lead conversation and tells it when one of its lanes changes', function () {
    $conversation = homeConversation(['conversation_id' => 'con_lead', 'title' => 'Hosted lead', 'lead' => true, 'spec' => 'spec.yaml', 'turns' => 0,
        'attention' => ['question' => 'Which repo should the widget fix go in?', 'set_at' => '2026-11-13T12:00:00Z', 'set_by' => 'cred_lead']]);
    Http::fake([
        director('models') => Http::response(['models' => [], 'default_model' => null]),
        director('conversations/con_lead/thread') => Http::response(['conversation' => $conversation, 'turns' => [], 'summaries' => []]),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_lead'])
        ->assertSee('data-test="lanes-pane"', false)
        ->assertSeeTextInOrder(['Needs your answer', 'Which repo should the widget fix go in?', 'Reply below to answer.']);

    $page->call('applyStream', [['kind' => 'job', 'id' => 'job_other', 'revision' => 3, 'record' => ['job_id' => 'job_other', 'lead_conversation_id' => 'con_else', 'revision' => 3]]])
        ->assertNotDispatched('thread-lanes-changed');
    $page->call('applyStream', [['kind' => 'job', 'id' => 'job_mine', 'revision' => 5, 'record' => ['job_id' => 'job_mine', 'lead_conversation_id' => 'con_lead', 'revision' => 5]]])
        ->assertDispatched('thread-lanes-changed');
});

it('shows no lanes pane on an ordinary conversation', function () {
    $conversation = homeConversation(['conversation_id' => 'con_chat', 'lead' => false, 'spec' => 'spec.yaml', 'turns' => 0]);
    Http::fake([
        director('models') => Http::response(['models' => [], 'default_model' => null]),
        director('conversations/con_chat/thread') => Http::response(['conversation' => $conversation, 'turns' => [], 'summaries' => []]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.conversation', ['conversation' => 'con_chat'])
        ->assertDontSee('data-test="lanes-pane"', false);
});

/**
 * A catalog with Claude Code models (which a lead can run on, `lead: true`) and others (which it cannot).
 *
 * @return array<string, mixed>
 */
function leadCatalog(): array
{
    return ['models' => [
        ['name' => 'codex', 'label' => 'Codex (subscription)', 'display_name' => 'Codex', 'provider' => 'openai', 'billing' => 'subscription', 'available' => true, 'lead' => false],
        ['name' => 'claude-code-sonnet', 'label' => 'Claude Sonnet (subscription)', 'display_name' => 'Claude Sonnet', 'provider' => 'anthropic', 'billing' => 'subscription', 'available' => true, 'lead' => true],
        ['name' => 'claude-code-opus', 'label' => 'Claude Opus (subscription)', 'display_name' => 'Claude Opus', 'provider' => 'anthropic', 'billing' => 'subscription', 'available' => true, 'lead' => true],
    ], 'default_model' => 'codex'];
}

function fakeLeadCreate(int $status = 201, array $answer = [], ?array $catalog = null): void
{
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'workstation', 'revoked_at' => null]]]),
        director('models') => Http::response($catalog ?? leadCatalog()),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
        director('conversations') => Http::response($answer ?: ['conversation' => homeConversation(['conversation_id' => 'con_new', 'lead' => true])], $status),
        director('conversations/con_new/turns') => Http::response(['turn' => ['turn_id' => 'trn_1', 'state' => 'queued']], 201),
    ]);
    fakeDirectorBackground();
}

it('starts a lead by default: no bundle, the hosted-lead spec, a Claude Code model, then the first message', function () {
    fakeLeadCreate();

    $page = Livewire::test('pages::client.conversation-create')
        ->assertSet('kind', 'lead')
        ->assertSet('model', 'claude-code-sonnet')
        ->assertDontSee('data-test="advanced-toggle"', false);

    expect(array_column($page->instance()->leadModelChoices(), 'name'))->toBe(['claude-code-sonnet', 'claude-code-opus']);

    $page->set('title', 'Desktop app')->set('message', 'Look at the lanes page.')->call('submit')
        ->assertRedirect(route('conversations.show', 'con_new'));

    Http::assertSent(fn ($request) => $request->method() === 'POST' && $request->url() === director('conversations')
        && $request->data() === ['node' => 'workstation', 'spec' => 'hosted-lead', 'model' => 'claude-code-sonnet', 'title' => 'Desktop app']
        && $request->hasHeader('Idempotency-Key'));
    Http::assertNotSent(fn ($request) => str_contains($request->url(), '/objects/'));
    Http::assertSent(fn ($request) => $request->url() === director('conversations/con_new/turns') && $request['message'] === 'Look at the lanes page.');
});

it('refuses a lead on a model the lead host cannot run', function () {
    fakeLeadCreate();

    Livewire::test('pages::client.conversation-create')->set('model', 'codex')->call('submit')
        ->assertHasErrors(['model' => 'in']);

    Http::assertNotSent(fn ($request) => $request->method() === 'POST' && $request->url() === director('conversations'));
});

it('switches back to a plain chat with the Director\'s default model', function () {
    fakeLeadCreate();

    Livewire::test('pages::client.conversation-create')
        ->set('kind', 'chat')
        ->assertSet('model', 'codex')
        ->assertSee('data-test="advanced-toggle"', false);
});

it('explains a Director whose release cannot start leads', function () {
    fakeLeadCreate(422, ['error' => 'Unprocessable', 'code' => 'lead_bundle_missing', 'detail' => 'no lead bundle']);

    Livewire::test('pages::client.conversation-create')->call('submit')
        ->assertSee('has no setup for conversations that run tasks');
});

it('treats a conversation marked as a lead as one, and shows its lanes pane', function () {
    $conversation = homeConversation(['conversation_id' => 'con_marked', 'lead' => true, 'spec' => 'spec.yaml', 'turns' => 0]);
    Http::fake([
        director('models') => Http::response(['models' => [], 'default_model' => null]),
        director('conversations/con_marked/thread') => Http::response(['conversation' => $conversation, 'turns' => [], 'summaries' => []]),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.conversation', ['conversation' => 'con_marked'])
        ->assertSee('data-test="lanes-pane"', false);
});

it('follows the catalog when it says which models can lead', function () {
    $catalog = leadCatalog();
    $catalog['models'][0]['lead'] = true;
    $catalog['models'][2]['lead'] = false;
    fakeLeadCreate(catalog: $catalog);

    $page = Livewire::test('pages::client.conversation-create');

    expect(array_column($page->instance()->leadModelChoices(), 'name'))->toBe(['codex', 'claude-code-sonnet']);
});

it('says plainly when the Director refuses a lead model, naming the ones it can run on', function () {
    fakeLeadCreate(422, ['error' => 'Unprocessable', 'code' => 'lead_model_unsupported', 'detail' => ['models' => ['claude-code-sonnet', 'claude-code-opus']]]);

    Livewire::test('pages::client.conversation-create')->call('submit')
        ->assertSeeText('That model cannot run tasks. These can: Claude Sonnet, Claude Opus.');
});

it('shows a model the Director lists more than once only once', function () {
    $catalog = leadCatalog();
    $catalog['models'][] = $catalog['models'][1];
    fakeLeadCreate(catalog: $catalog);

    $page = Livewire::test('pages::client.conversation-create');

    expect(array_column($page->instance()->leadModelChoices(), 'name'))->toBe(['claude-code-sonnet', 'claude-code-opus']);
});

it('refuses an empty or overlong chief-of-staff name, asking the Director nothing', function (string $name) {
    meOnDirector(['credential_id' => 'cred_local', 'owner' => true]);
    Http::fake([
        director('lead-node') => Http::response(['error' => 'Not Found', 'code' => 'not_found'], 404),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->set('chiefOfStaff', true)
        ->set('chiefOfStaffName', $name)
        ->call('saveChiefOfStaff')
        ->assertHasErrors('chiefOfStaffName');

    Http::assertNotSent(fn ($request) => $request->method() === 'PUT');
})->with(['empty' => [''], 'too long' => [str_repeat('a', 41)]]);

it('keeps the chief of staff as it was and says why when the Director refuses, as it does for a paired device', function () {
    meOnDirector(['credential_id' => 'cred_phone', 'owner' => false]);
    Http::fake([
        director('lead-node') => Http::response(['error' => 'Not Found', 'code' => 'not_found'], 404),
        director('chief-of-staff') => Http::response(['error' => 'Forbidden', 'code' => 'owner_only', 'detail' => 'only the owner can change the chief of staff'], 403),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')->set('chiefOfStaff', true)->call('saveChiefOfStaff')
        ->assertNoRedirect()->assertSee('data-test="chief-of-staff-problem"', false);
});

it('opens the conversation list, not an error page, when the Director cannot answer', function (int $status) {
    Http::fake([director('conversations?*') => Http::response(['error' => 'Bad Gateway', 'code' => 'unavailable'], $status)]);
    fakeDirectorBackground();

    $this->get(route('home'))->assertRedirect(route('conversations.index'));
})->with([502, 503]);

it('keeps a long-lived lead on the sidebar however many newer conversations there are', function () {
    Http::fake([
        director('conversations?lead=1*') => Http::response(['conversations' => [
            homeConversation(['conversation_id' => 'con_lead', 'title' => 'Old lead', 'lead' => true, 'created_at' => '2026-01-01T00:00:00Z']),
        ]]),
        director('conversations?*') => Http::response(['conversations' => [homeConversation(['conversation_id' => 'con_new', 'title' => 'newer chat'])]]),
    ]);
    fakeDirectorBackground();

    Livewire::withoutLazyLoading()->test('client.thread-rail')->assertSeeTextInOrder(['Old lead', 'newer chat']);
    Http::assertSent(fn ($request) => $request->url() === director('conversations?limit=100'));
});

it('lists a page of conversations at a time, with "Show more" while there are older ones', function () {
    $conversations = array_map(fn (int $n): array => homeConversation(['conversation_id' => "con_{$n}", 'title' => "chat {$n}",
        'created_at' => sprintf('2026-11-%02dT00:00:00Z', 30 - intdiv($n, 3))]), range(1, 51));
    Http::fake(fn ($request) => Http::response(['conversations' => array_slice($conversations, 0, (int) $request->data()['limit'])]));
    fakeDirectorBackground();

    $page = Livewire::test('pages::client.conversations')->assertSee('data-test="show-more"', false);
    expect($page->instance()->conversations())->toHaveCount(50);
    Http::assertSent(fn ($request) => $request->url() === director('conversations?limit=51'));

    $page->call('showMore')->assertDontSee('data-test="show-more"', false);
    expect($page->instance()->conversations())->toHaveCount(51);
});

it('reads and saves the chief of staff on the Director, so every client shows the same', function () {
    chiefOfStaffOnDirector(['enabled' => false, 'name' => 'Ada', 'conversation_id' => 'con_chief']);
    meOnDirector(['credential_id' => 'cred_local', 'owner' => true]);
    Http::fake([
        director('lead-node') => Http::response(['node' => 'workstation', 'source' => 'set']),
        director('chief-of-staff') => Http::response(['chief_of_staff' => ['enabled' => true, 'name' => 'Ada', 'conversation_id' => 'con_chief', 'revision' => 2]]),
    ]);
    fakeDirectorBackground();

    Livewire::test('pages::client.connect')
        ->assertSet('chiefOfStaff', false)->assertSet('chiefOfStaffName', 'Ada')
        ->set('chiefOfStaff', true)
        ->call('saveChiefOfStaff')
        ->assertRedirect(route('conversations.show', 'con_chief'));

    Http::assertSent(fn ($request) => $request->method() === 'PUT' && $request->url() === director('chief-of-staff')
        && $request['enabled'] === true && $request['name'] === 'Ada' && $request->hasHeader('Idempotency-Key'));
    // The Director starts or brings back the conversation; the app does not.
    Http::assertNotSent(fn ($request) => $request->method() === 'POST' && $request->url() === director('conversations'));
});

it('opens on, pins and protects the chief of staff the Director names', function () {
    chiefOfStaffOnDirector(['enabled' => true, 'name' => 'Ada', 'conversation_id' => 'con_chief']);
    $chief = conversationRecord(['conversation_id' => 'con_chief', 'title' => 'Ada', 'lead' => true]);
    Http::fake([
        director('conversations?*') => Http::response(['conversations' => [$chief, homeConversation(['title' => 'planning chat'])]]),
        director('conversations/con_chief/thread') => Http::response(['conversation' => $chief, 'turns' => [], 'summaries' => []]),
        director('models') => Http::response(['models' => []]),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
    ]);
    fakeDirectorBackground();

    $this->get(route('home'))->assertRedirect(route('conversations.show', 'con_chief'));
    $rail = Livewire::withoutLazyLoading()->test('client.thread-rail')->assertSee('data-test="rail-chief-of-staff"', false);
    expect(array_column($rail->get('threads'), 'id'))->toBe(['con_chat']);
    Livewire::test('pages::client.conversation', ['conversation' => 'con_chief'])
        ->assertSet('chiefName', 'Ada')->assertDontSee('data-test="conversation-menu"', false);
});

it('says why when the Director refuses to archive the chief of staff from a stale page', function () {
    fakeThread(conversationRecord(), [turnRecord()], [], [
        director('conversations/con_1/archive') => Http::response(['error' => 'Conflict', 'code' => 'chief_of_staff', 'detail' => 'this conversation is the chief of staff'], 409),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])->call('archive')
        ->assertSee('This is the chief of staff. Turn it off in Settings to archive it.');
});
