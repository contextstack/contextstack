<?php

use Carbon\CarbonImmutable;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * @return array<string, mixed>
 */
function conversationRecord(array $over = []): array
{
    return [...['conversation_id' => 'con_1', 'title' => 'planning chat', 'node' => 'laptop', 'spec' => 'spec.yaml', 'spec_version' => str_repeat('c', 64),
        'model' => 'local-coder', 'intent_id' => null, 'project_id' => null, 'held' => false, 'state' => 'idle', 'active_turn_id' => null,
        'queued_turn_ids' => [], 'turns' => 0, 'summaries' => 0, 'attention' => null,
        'budget' => ['state' => 'none', 'shown' => 'no budget set', 'usd' => null],
        'compaction' => ['context_window' => 32768, 'window_source' => 'table', 'threshold_tokens' => 19000, 'keep_tokens' => 9500],
        'created_by' => 'cred_local', 'created_at' => '2026-11-10T10:00:00Z', 'revision' => 10], ...$over];
}

/**
 * @return array<string, mixed>
 */
function turnRecord(array $over = []): array
{
    return [...['turn_id' => 'trn_1', 'conversation_id' => 'con_1', 'seq' => 1, 'message' => 'Hello there.', 'state' => 'done',
        'posted_by' => 'cred_local', 'posted_by_name' => 'laptop', 'posted_at' => '2026-11-10T10:00:01Z', 'job_id' => 'job_t1', 'run_id' => 'run_t1',
        'reply' => 'General Kenobi.', 'detail' => null, 'cancel' => null, 'started_at' => '2026-11-10T10:00:01Z', 'finished_at' => '2026-11-10T10:00:09Z',
        'context' => ['conversation_revision' => 11, 'tokens' => 120, 'over_threshold' => false, 'summary_id' => null, 'summary_through_seq' => null,
            'whole_from_seq' => null, 'whole_through_seq' => null],
        'revision' => 12], ...$over];
}

/**
 * @return array<string, mixed>
 */
function summaryRecord(array $over = []): array
{
    return [...['summary_id' => 'sum_1', 'conversation_id' => 'con_1', 'through_seq' => 2, 'based_on' => null, 'redoes' => null,
        'job_id' => 'job_s1', 'run_id' => 'run_s1', 'state' => 'done', 'text' => 'They greeted each other twice.', 'detail' => null,
        'requested_by' => 'director', 'created_at' => '2026-11-10T10:01:00Z', 'finished_at' => '2026-11-10T10:01:05Z', 'revision' => 20], ...$over];
}

/**
 * @return array<string, mixed>
 */
function attentionRecord(array $over = []): array
{
    return [...['question' => 'Approve merge?', 'job_id' => 'job_attn', 'set_at' => '2026-11-10T12:00:00Z', 'set_by' => 'cred_lead', 'source_kind' => 'lead'], ...$over];
}

function fakeThread(array $conversation, array $turns, array $summaries = [], array $more = []): void
{
    Http::fake([
        director('models') => Http::response(modelsCatalog()),
        director('conversations/con_1/thread') => Http::response(['conversation' => $conversation, 'turns' => $turns, 'summaries' => $summaries]),
        ...$more,
    ]);
}

it('starts a built-in chat attached to a task, naming the node and model, uploading nothing, and opens it', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response([
            'models' => [
                ['name' => 'local-coder', 'label' => 'Local coder (on this Mac)', 'display_name' => 'Local coder', 'provider' => 'ollama', 'billing' => 'free_local', 'available' => true, 'cost_class' => 'free_local'],
                ['name' => 'codex', 'label' => 'Codex (subscription)', 'display_name' => 'Codex', 'provider' => 'openai', 'billing' => 'subscription', 'available' => true, 'cost_class' => 'subscription'],
            ],
            'default_model' => 'codex',
        ]),
        director('intents?*') => Http::response(['intents' => [['intent_id' => 'int_1', 'title' => 'say hello', 'task' => 'tasks/hello']], 'jobs' => []]),
        director('projects') => Http::response(['projects' => [['project_id' => 'prj_1', 'name' => 'site']]]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['conversation' => conversationRecord()], 201),
    ]);

    $page = Livewire::test('pages::client.conversation-create')->set('showAdvanced', true)
        ->assertSee('say hello')->assertSee('site')
        ->set('title', 'planning chat')->set('intentId', 'int_1')->set('projectId', 'prj_1');
    $key = $page->get('submitKey');
    $page->call('submit')->assertHasNoErrors()->assertRedirect(route('conversations.show', 'con_1'));

    // "Attach to a task" offers the newest tasks only, not every task the Director holds.
    Http::assertSent(fn (Request $request) => $request->method() === 'GET' && $request->url() === director('intents?limit=100'));
    Http::assertSent(function (Request $request) use ($key) {
        if ($request->method() !== 'POST' || $request->url() !== director('conversations')) {
            return false;
        }

        // An attached conversation takes its intent's project, so none is named.
        return $request->hasHeader('Idempotency-Key', $key) && $request['node'] === 'laptop' && $request['spec'] === 'chat'
            && $request['model'] === 'codex' && ! isset($request['files'])
            && $request['intent_id'] === 'int_1' && ! isset($request['project_id']) && $request['title'] === 'planning chat';
    });
    Http::assertNotSent(fn (Request $request) => str_contains($request->url(), 'objects/'));
});

it('names the project for a conversation that stands alone, and shows the Director\'s reason for an unusable spec', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response([
            'models' => [
                ['name' => 'local-coder', 'label' => 'Local coder (on this Mac)', 'display_name' => 'Local coder', 'provider' => 'ollama', 'billing' => 'free_local', 'available' => true, 'cost_class' => 'free_local'],
                ['name' => 'codex', 'label' => 'Codex (subscription)', 'display_name' => 'Codex', 'provider' => 'openai', 'billing' => 'subscription', 'available' => true, 'cost_class' => 'subscription'],
            ],
            'default_model' => 'codex',
        ]),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => [['project_id' => 'prj_1', 'name' => 'site']]]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['error' => 'Unprocessable', 'code' => 'invalid_request', 'detail' => 'the spec must list tasks/turn in tasks'], 422),
    ]);

    Livewire::test('pages::client.conversation-create')->set('showAdvanced', true)->set('projectId', 'prj_1')->call('submit')
        ->assertSee('the spec must list tasks/turn in tasks');

    Http::assertSent(fn (Request $request) => $request->url() === director('conversations') && $request['project_id'] === 'prj_1');
});

it('lists conversations newest first with their state and budget', function () {
    Http::fake([director('conversations?*') => Http::response(['conversations' => [
        conversationRecord(['conversation_id' => 'con_old', 'title' => 'older', 'created_at' => '2026-11-09T10:00:00Z']),
        conversationRecord(['title' => 'newer', 'state' => 'held', 'held' => true, 'queued_turn_ids' => ['trn_2']]),
    ]])]);

    $this->get(route('conversations.index'))->assertOk()
        ->assertSeeInOrder(['newer', 'older'])->assertSee('stopped: messages queue')->assertSee('1 queued')->assertSee('no budget set');
});

it('shows the whole thread, with each summary shown as a summary after the turns it covers', function () {
    fakeThread(conversationRecord(['turns' => 3, 'summaries' => 1]), [
        turnRecord(),
        turnRecord(['turn_id' => 'trn_2', 'seq' => 2, 'message' => 'Again.', 'reply' => 'Hello again.']),
        turnRecord(['turn_id' => 'trn_3', 'seq' => 3, 'message' => 'Third.', 'reply' => 'Third reply.',
            'context' => ['summary_id' => 'sum_1', 'summary_through_seq' => 2, 'whole_from_seq' => 3, 'whole_through_seq' => 2, 'tokens' => 80, 'over_threshold' => false]]),
    ], [summaryRecord()]);

    $page = $this->get(route('conversations.show', 'con_1'))->assertOk();

    $page->assertSeeInOrder(['Hello there.', 'General Kenobi.', 'Again.', 'Hello again.', 'Summary of messages 1–2', 'They greeted each other twice.', 'Third.', 'Third reply.'])
        ->assertSee('Sent a summary of messages 1–2, then')->assertSee('Regenerate summary');
    expect(substr_count($page->getContent(), 'data-test="turn"'))->toBe(3)
        ->and(substr_count($page->getContent(), 'data-test="summary"'))->toBe(1);
});

it('answers 404 for a conversation the Director does not know', function () {
    Http::fake([director('conversations/nope/thread') => Http::response(['error' => 'Not Found', 'code' => 'not_found', 'detail' => 'no such conversation'], 404)]);

    $this->get(route('conversations.show', 'nope'))->assertNotFound();
});

it('posts a turn with a key, shows it queued behind the active turn, and keeps the key when the answer is lost', function () {
    $running = turnRecord(['state' => 'running', 'reply' => null, 'run_id' => null, 'finished_at' => null]);
    $queued = turnRecord(['turn_id' => 'trn_2', 'seq' => 2, 'message' => 'From the phone.', 'state' => 'queued', 'job_id' => null, 'run_id' => null,
        'reply' => null, 'context' => null, 'posted_by_name' => 'phone', 'revision' => 30]);
    $posts = 0;
    Http::fake(function (Request $request) use (&$posts, $running, $queued) {
        return match (true) {
            str_ends_with($request->url(), '/thread') => Http::response(['conversation' => conversationRecord(['state' => 'running', 'active_turn_id' => 'trn_1']), 'turns' => [$running], 'summaries' => []]),
            str_contains($request->url(), 'jobs/job_t1') => Http::response(['job_id' => 'job_t1', 'state' => 'accepted', 'live_run_ids' => [], 'conversation_id' => 'con_1', 'revision' => 13]),
            $request->url() === director('conversations/con_1/turns') && ++$posts === 1 => throw new ConnectionException('timed out'),
            $request->url() === director('conversations/con_1/turns') => Http::response(['turn' => $queued,
                'conversation' => conversationRecord(['state' => 'running', 'queued_turn_ids' => ['trn_2'], 'revision' => 31])], 201),
            default => Http::response([], 404),
        };
    });

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1']);
    $key = $page->get('postKey');

    $page->set('message', 'From the phone.')->call('post')->assertSee('Sending again is safe')->assertSet('postKey', $key)->assertSet('message', 'From the phone.');
    $page->call('post')->assertSee('Queued behind the current reply.')->assertSee('From the phone.')->assertSee('1 message queued')->assertSet('message', '');

    expect($page->get('postKey'))->not->toBe($key);
    $keys = Http::recorded()->filter(fn (array $pair) => $pair[0]->method() === 'POST')->map(fn (array $pair) => $pair[0]->header('Idempotency-Key')[0]);
    expect($keys->unique()->values()->all())->toBe([$key]);
});

it('refuses an empty message before asking the Director', function () {
    fakeThread(conversationRecord(), []);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])->call('post')->assertHasErrors(['message' => 'required']);
    Http::assertNotSent(fn (Request $request) => $request->method() === 'POST');
});

it('shows a reply as it is written, from run events tagged with the turn, until the finished reply replaces it', function () {
    $running = turnRecord(['state' => 'running', 'reply' => null, 'run_id' => null]);
    fakeThread(conversationRecord(['state' => 'running', 'active_turn_id' => 'trn_1']), [$running], [], [
        director('jobs/job_t1') => Http::response(['job_id' => 'job_t1', 'state' => 'accepted', 'live_run_ids' => ['run_t1'], 'conversation_id' => 'con_1', 'revision' => 13]),
        director('runs/run_t1/events*') => Http::response(['run_id' => 'run_t1', 'finished' => false, 'confirmed' => 1,
            'events' => [['seq' => 1, 'kind' => 'delta', 'partial' => true, 'payload' => ['text' => 'Gen']]]]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])->assertSee('Gen');

    $page->call('applyStream', [], [
        ['run_id' => 'run_t1', 'job_id' => 'job_t1', 'conversation_id' => 'con_1', 'turn_id' => 'trn_1', 'seq' => 1, 'kind' => 'delta', 'payload' => ['text' => 'Gen']],
        ['run_id' => 'run_t1', 'job_id' => 'job_t1', 'conversation_id' => 'con_1', 'turn_id' => 'trn_1', 'seq' => 2, 'kind' => 'delta', 'payload' => ['text' => 'eral Ken']],
        ['run_id' => 'run_x', 'job_id' => 'job_x', 'conversation_id' => 'con_other', 'turn_id' => 'trn_x', 'seq' => 1, 'kind' => 'delta', 'payload' => ['text' => 'not ours']],
    ])->assertSee('General Ken')->assertSee('the finished reply replaces this')->assertDontSee('not ours');

    $page->call('applyStream', [['kind' => 'turn', 'id' => 'trn_1', 'revision' => 40, 'record' => turnRecord(['reply' => 'General Kenobi, you are a bold one.', 'revision' => 40])]])
        ->assertSee('General Kenobi, you are a bold one.')->assertDontSee('the finished reply replaces this');
    expect(substr_count($page->html(), 'General Ken'))->toBe(1);
});

it('applies only changes to this conversation, and only when newer than the record held', function () {
    fakeThread(conversationRecord(), [turnRecord()]);
    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1']);

    $page->call('applyStream', [
        ['kind' => 'turn', 'id' => 'trn_1', 'revision' => 5, 'record' => turnRecord(['reply' => 'stale', 'revision' => 5])],
        ['kind' => 'turn', 'id' => 'trn_9', 'revision' => 50, 'record' => turnRecord(['turn_id' => 'trn_9', 'conversation_id' => 'con_other', 'message' => 'elsewhere', 'revision' => 50])],
    ])->assertSee('General Kenobi.')->assertDontSee('stale')->assertDontSee('elsewhere');
});

it('stops with a key, showing the queue held and the turn stopping, then resumes with another key', function () {
    $cancel = ['cancel_id' => 'can_1', 'state' => 'requested', 'outcome' => null];
    Http::fake([
        director('conversations/con_1/thread') => Http::response(['conversation' => conversationRecord(['state' => 'running', 'active_turn_id' => 'trn_1']),
            'turns' => [turnRecord(['state' => 'running', 'reply' => null])], 'summaries' => []]),
        director('jobs/job_t1') => Http::response(['job_id' => 'job_t1', 'state' => 'accepted', 'live_run_ids' => [], 'conversation_id' => 'con_1', 'revision' => 13]),
        director('conversations/con_1/stop') => Http::response(['conversation' => conversationRecord(['state' => 'held', 'held' => true, 'revision' => 40]),
            'cancel' => [...$cancel, 'job_id' => 'job_t1', 'revision' => 40]], 202),
        director('conversations/con_1/resume') => Http::response(['conversation' => conversationRecord(['state' => 'idle', 'held' => false, 'revision' => 50]), 'cancel' => null]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1']);
    $stopKey = $page->get('stopKey');
    $resumeKey = $page->get('resumeKey');

    $page->call('stop')->assertSee('the job for the current message is asked to stop')->assertSee('stopped: messages queue')->assertSee('Resume');
    $page->call('applyStream', [['kind' => 'turn', 'id' => 'trn_1', 'revision' => 41, 'record' => turnRecord(['state' => 'running', 'reply' => null, 'cancel' => $cancel, 'revision' => 41])]])
        ->assertSee('stopping');
    $page->call('applyStream', [['kind' => 'turn', 'id' => 'trn_1', 'revision' => 45, 'record' => turnRecord(['state' => 'stopped', 'reply' => null, 'detail' => 'stopped by request',
        'cancel' => ['cancel_id' => 'can_1', 'state' => 'applied', 'outcome' => 'stopped'], 'revision' => 45])]])
        ->assertSee('stopped by request');
    $page->call('resume')->assertSee('Resumed: the next queued message starts.')->assertDontSee('stopped: messages queue');

    expect($stopKey)->not->toBe($resumeKey);
    Http::assertSent(fn (Request $request) => $request->url() === director('conversations/con_1/stop') && $request->hasHeader('Idempotency-Key', $stopKey));
    Http::assertSent(fn (Request $request) => $request->url() === director('conversations/con_1/resume') && $request->hasHeader('Idempotency-Key', $resumeKey));
});

it('regenerates a summary with a key, and says so when that summary is still being made', function () {
    fakeThread(conversationRecord(), [turnRecord(), turnRecord(['turn_id' => 'trn_2', 'seq' => 2])], [summaryRecord()], [
        director('conversations/con_1/summaries/sum_1/redo') => Http::sequence()
            ->push(['summary' => summaryRecord(['summary_id' => 'sum_2', 'redoes' => 'sum_1', 'state' => 'running', 'text' => null, 'run_id' => null, 'job_id' => 'job_s2', 'revision' => 60])], 202)
            ->push(['error' => 'Conflict', 'code' => 'summary_running', 'detail' => 'still being made'], 409),
        director('jobs/job_s2') => Http::response(['job_id' => 'job_s2', 'state' => 'offered', 'live_run_ids' => [], 'conversation_id' => 'con_1', 'revision' => 61]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1']);
    $key = $page->get('redoKey');

    $page->call('redo', 'sum_1')->assertSee('A new summary of the same messages is being made.')->assertSee('redo of sum_1')->assertSee('replaced by the redo sum_2')
        ->assertSee('Being made by an ordinary job');
    Http::assertSent(fn (Request $request) => $request->url() === director('conversations/con_1/summaries/sum_1/redo') && $request->hasHeader('Idempotency-Key', $key));

    $page->call('redo', 'sum_1')->assertSee('That summary is still being made; redo it once it ends.');
    $page->call('redo', 'sum_unknown')->assertStatus(404);
});

it('shows a turn held back by a budget with the Director\'s reason', function () {
    fakeThread(conversationRecord(['state' => 'running', 'budget' => ['state' => 'exhausted', 'shown' => 'budget used up', 'usd' => 0.0, 'period' => 'total',
        'spent_usd' => 0.0, 'reserved_usd' => 0.0, 'remaining_usd' => 0.0, 'resets_at' => null, 'waiting_job_ids' => ['job_t1']]]),
        [turnRecord(['state' => 'running', 'reply' => null, 'run_id' => null])], [], [
            director('jobs/job_t1') => Http::response(['job_id' => 'job_t1', 'state' => 'waiting', 'conversation_id' => 'con_1', 'live_run_ids' => [],
                'budget' => ['state' => 'waiting', 'reason' => 'budget_exhausted', 'detail' => 'budget used up: the conversation\'s $0.00 is spent'], 'revision' => 13]),
        ]);

    $this->get(route('conversations.show', 'con_1'))->assertOk()
        ->assertSee('budget used up: the conversation&#039;s $0.00 is spent', false)->assertSee('data-state="exhausted"', false);
});

it('sets and clears the conversation\'s budget, with no key since a budget write replaces it', function () {
    fakeThread(conversationRecord(), [], [], [
        director('conversations/con_1/budget') => Http::sequence()
            ->push(['conversation' => conversationRecord(['budget' => ['state' => 'active', 'shown' => '$2.00 of $2.00 left this day', 'usd' => 2.0, 'period' => 'day',
                'spent_usd' => 0.0, 'reserved_usd' => 0.0, 'remaining_usd' => 2.0, 'resets_at' => '2026-11-11T05:00:00Z', 'waiting_job_ids' => []], 'revision' => 70])])
            ->push(['conversation' => conversationRecord(['revision' => 71])]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])->assertSee('no budget set');
    $page->set('budget.usd', '2')->set('budget.period', 'day')->call('setBudget')->assertSee('$2.00 of $2.00 left this day')->assertSee('Clear budget');
    $page->call('clearBudget')->assertSee('no budget set');

    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->url() === director('conversations/con_1/budget')
        && $request['usd'] === '2' && $request['period'] === 'day' && ! $request->hasHeader('Idempotency-Key'));
    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && $request->url() === director('conversations/con_1/budget'));
});

it('relays only this conversation\'s events when the page asks for them', function () {
    Http::fake([director('events*') => Http::response("event: hello\ndata: {\"revision\": 1, \"conversation\": \"con_1\"}\n\n", 200, ['Content-Type' => 'text/event-stream'])]);

    $this->get(route('stream', ['conversation' => 'con_1']))->streamedContent();
    $this->get(route('stream', ['conversation' => '../bad id']))->streamedContent();

    Http::assertSent(fn (Request $request) => $request->url() === director('events?conversation=con_1'));
    Http::assertSent(fn (Request $request) => $request->url() === director('events'));
});

it('lists needs-you first with the question shown, and hides the badge when attention is cleared', function () {
    Http::fake([director('conversations?*') => Http::response(['conversations' => [
        conversationRecord(['conversation_id' => 'con_new', 'title' => 'newer', 'created_at' => '2026-11-10T10:00:00Z', 'attention' => null]),
        conversationRecord(['conversation_id' => 'con_old', 'title' => 'older needs you', 'created_at' => '2026-11-09T10:00:00Z',
            'attention' => attentionRecord(['question' => 'Which branch?'])]),
    ]])]);

    $page = $this->get(route('conversations.index'))->assertOk()
        ->assertSeeInOrder(['older needs you', 'newer'])
        ->assertSee('Needs you')
        ->assertSee('asked by the agent in con_old')
        ->assertSee('Which branch?');

    expect(substr_count($page->getContent(), 'data-test="needs-you-badge"'))->toBe(1);

    $page = Livewire::test('pages::client.conversations')->call('applyStream', [
        ['kind' => 'conversation', 'id' => 'con_old', 'revision' => 20,
            'record' => conversationRecord(['conversation_id' => 'con_old', 'title' => 'older needs you', 'created_at' => '2026-11-09T10:00:00Z', 'revision' => 20, 'attention' => null])],
    ])->assertDontSee('Which branch?');

    expect(substr_count($page->html(), 'data-test="needs-you-badge"'))->toBe(0);
});

it('shows the attention question and job link, dismiss clears via DELETE, and posting clears the banner', function () {
    Http::fake(function (Request $request) {
        return match (true) {
            str_ends_with($request->url(), '/thread') => Http::response([
                'conversation' => conversationRecord(['intent_id' => 'int_1', 'attention' => attentionRecord()]),
                'turns' => [],
                'summaries' => [],
            ]),
            $request->method() === 'DELETE' && str_ends_with($request->url(), '/attention') => Http::response([
                'conversation' => conversationRecord(['intent_id' => 'int_1', 'attention' => null, 'revision' => 50]),
            ]),
            $request->url() === director('conversations/con_1/turns') => Http::response([
                'turn' => turnRecord(['state' => 'queued']),
                'conversation' => conversationRecord(['intent_id' => 'int_1', 'attention' => null, 'revision' => 61]),
            ], 201),
            default => Http::response([], 404),
        };
    });

    $this->get(route('conversations.show', 'con_1'))->assertOk()
        ->assertSee('Approve merge?')
        ->assertSee('data-test="attention-message"', false)
        ->assertSee('Reply below to answer.')
        ->assertSee(route('lanes.show', 'job_attn'), false);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->assertSee('Approve merge?')
        ->call('dismissAttention')
        ->assertDontSee('data-test="attention-message"', false)
        ->assertDontSee('Approve merge?');

    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && $request->url() === director('conversations/con_1/attention'));

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->call('applyStream', [
            ['kind' => 'conversation', 'id' => 'con_1', 'revision' => 60,
                'record' => conversationRecord(['intent_id' => 'int_1', 'revision' => 60, 'attention' => attentionRecord(['question' => 'Ready to land?'])]),
            ],
        ])->assertSee('Ready to land?');

    $page->set('message', 'Yes.')->call('post')->assertDontSee('Ready to land?');
});

it('renders Markdown replies with emphasis, lists, code and safe links', function () {
    Http::preventStrayRequests();
    fakeThread(conversationRecord(), [turnRecord(['reply' => "**bold**\n\n- first\n- second\n\n```php\necho 'hello';\n```\n\nUse `inline` and [Laravel](https://laravel.com)."])]);

    $this->get(route('conversations.show', 'con_1'))->assertOk()
        ->assertSee('<strong>bold</strong>', false)
        ->assertSee("<ul>\n<li>first</li>\n<li>second</li>\n</ul>", false)
        ->assertSee('<pre class="cs-coloured"><code class="language-php">', false)
        ->assertSee('echo')
        ->assertSee('<code>inline</code>', false)
        ->assertSee('<a href="https://laravel.com">Laravel</a>', false);
});

it('keeps unsafe reply HTML and links inert', function () {
    Http::preventStrayRequests();
    fakeThread(conversationRecord(), [turnRecord(['reply' => '<script>alert(1)</script>'."\n\n".'[unsafe](javascript:alert%281%29)'."\n\n".'<img src=x onerror=alert(1)>'])]);

    $this->get(route('conversations.show', 'con_1'))->assertOk()
        ->assertSee('&lt;script&gt;alert(1)&lt;/script&gt;', false)
        ->assertSee('&lt;img src=x onerror=alert(1)&gt;', false)
        ->assertSee('unsafe')
        ->assertDontSee('<script>alert(1)</script>', false)
        ->assertDontSee('<img src=x', false)
        ->assertDontSee('href="javascript:', false);
});

it('renders a person\'s Markdown safely, keeping their line breaks', function () {
    Http::preventStrayRequests();
    fakeThread(conversationRecord(), [turnRecord(['message' => "**user bold** and `Cursor`\nsecond line <script>alert(1)</script> <img src=x onerror=alert(1)>"])]);

    $this->get(route('conversations.show', 'con_1'))->assertOk()
        ->assertSee('<strong>user bold</strong> and <code>Cursor</code><br />', false)
        ->assertSee('second line &lt;script&gt;alert(1)&lt;/script&gt; &lt;img src=x onerror=alert(1)&gt;', false)
        ->assertDontSee('<script>alert(1)</script>', false)
        ->assertDontSee('<img src=x', false);
});

it('frames a code block with its language and a copy button, and wraps long lines', function () {
    Http::preventStrayRequests();
    fakeThread(conversationRecord(), [turnRecord(['reply' => "Here:\n\n```php\necho 'hello';\n```", 'state' => 'done'])]);

    $this->get(route('conversations.show', 'con_1'))->assertOk()
        ->assertSeeInOrder(['data-code-block', '<span>php</span>', 'data-test="copy-code"', '<pre class="cs-coloured"><code class="language-php">', 'echo', '</code></pre></div>'], false);
});

it('renders safe Markdown in attention banners and list excerpts', function (string $route) {
    Http::preventStrayRequests();
    $conversation = conversationRecord(['attention' => attentionRecord(['question' => '**Decision** [details](https://example.com)'."\n\n".'<script>alert(1)</script>'."\n\n".'[unsafe](javascript:alert%281%29)'."\n\n".'<img src=x onerror=alert(1)>'])]);
    fakeThread($conversation, [], [], [director('conversations?*') => Http::response(['conversations' => [$conversation]])]);

    $this->get(route($route, $route === 'conversations.show' ? ['conversation' => 'con_1'] : []))->assertOk()
        ->assertSee('<strong>Decision</strong>', false)
        ->assertSee('<a href="https://example.com">details</a>', false)
        ->assertSee('&lt;script&gt;alert(1)&lt;/script&gt;', false)
        ->assertSee('&lt;img src=x onerror=alert(1)&gt;', false)
        ->assertDontSee('<script>alert(1)</script>', false)
        ->assertDontSee('<img src=x', false)
        ->assertDontSee('href="javascript:', false);
})->with(['conversations.show', 'conversations.index']);

it('keeps owner attention distinct from hosted lead attention', function () {
    fakeThread(conversationRecord(['attention' => attentionRecord(['source_kind' => 'local'])]), [], []);

    $this->get(route('conversations.show', 'con_1'))
        ->assertSee('Approve merge?')
        ->assertDontSee('asked by the agent in');
});

function modelsCatalog(): array
{
    return [
        'models' => [
            ['name' => 'local-coder', 'label' => 'Local coder (on this Mac)', 'display_name' => 'Local coder', 'provider' => 'ollama', 'billing' => 'free_local', 'available' => true, 'cost_class' => 'free_local'],
            ['name' => 'codex', 'label' => 'Codex (subscription)', 'display_name' => 'Codex', 'provider' => 'openai', 'billing' => 'subscription', 'available' => true, 'cost_class' => 'subscription'],
        ],
        'default_model' => 'codex',
    ];
}

it('creates the Director\'s built-in chat on the chosen model from the simple form', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(modelsCatalog()),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['conversation' => conversationRecord(['model' => 'codex'])], 201),
    ]);

    Livewire::test('pages::client.conversation-create')
        ->assertSet('model', 'codex')
        ->assertSet('provider', 'openai')
        ->assertSee('Codex (subscription) · subscription')
        ->set('model', 'local-coder')
        ->assertSet('provider', 'ollama')
        ->call('submit')
        ->assertHasNoErrors()
        ->assertRedirect(route('conversations.show', 'con_1'));

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('conversations')
        && $request['spec'] === 'chat' && $request['model'] === 'local-coder' && $request['node'] === 'laptop' && ! isset($request['files']));
});

it('asks for a spec before sending a system prompt or files of your own', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(modelsCatalog()),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
    ]);

    Livewire::test('pages::client.conversation-create')->set('kind', 'chat')->set('showAdvanced', true)
        ->set('system', 'Be terse.')->call('submit')->assertHasErrors('spec');
    Http::assertNotSent(fn (Request $request) => $request->method() === 'POST');
});

it('still creates a conversation from advanced fields with a custom spec', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(modelsCatalog()),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['conversation' => conversationRecord(['model' => 'fake'])], 201),
    ]);

    $custom = "id: chat\nmodel: fake\nsystem: prompts/system.md\ncontext: [conversation/context.md]\ntools: []\nmcp: []\n".
        "budget: {turns: 4, tool_calls: 0, usd: 0, seconds: 300}\ntasks: [tasks/turn]\ntrials: 1\n";

    Livewire::test('pages::client.conversation-create')
        ->set('showAdvanced', true)
        ->set('spec', $custom)
        ->call('submit')
        ->assertRedirect(route('conversations.show', 'con_1'));

    Http::assertSent(fn (Request $request) => $request->url() === director('conversations'));
});

it('posts the first message when starting a conversation', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(modelsCatalog()),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['conversation' => conversationRecord()], 201),
        director('conversations/con_1/turns') => Http::response([
            'turn' => turnRecord(['state' => 'running', 'reply' => null]),
            'conversation' => conversationRecord(['state' => 'running', 'active_turn_id' => 'trn_1']),
        ], 201),
    ]);

    $page = Livewire::test('pages::client.conversation-create')
        ->set('message', 'Hello there.');
    $postKey = $page->get('postKey');
    $page->call('submit')
        ->assertRedirect(route('conversations.show', 'con_1'));
    Http::assertSent(fn (Request $request) => $request->url() === director('conversations/con_1/turns')
        && $request['message'] === 'Hello there.'
        && $request->hasHeader('Idempotency-Key', $postKey));
});

it('shows ready and unavailable models in the picker with reasons', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response([
            'models' => [
                ['name' => 'codex', 'display_name' => 'Codex', 'provider' => 'openai', 'billing' => 'subscription', 'available' => true, 'cost_class' => 'subscription'],
                ['name' => 'claude-code', 'display_name' => 'Claude Code', 'provider' => 'anthropic', 'billing' => 'subscription', 'available' => false, 'cost_class' => 'subscription', 'unavailable_reason' => 'sign in with `claude` first'],
            ],
            'default_model' => 'codex',
        ]),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
    ]);

    Livewire::test('pages::client.conversation-create')
        ->assertSet('model', 'codex')
        ->assertSet('canStart', true)
        ->assertSet('provider', 'openai')
        ->assertSee('Codex · subscription')
        ->set('provider', 'anthropic')
        ->assertSet('model', '')
        ->assertSee('Claude Code · subscription — sign in with `claude` first');
});

it('defaults to the first ready subscription model when the Director default is unavailable', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response([
            'models' => [
                ['name' => 'claude-code', 'display_name' => 'Claude Code', 'provider' => 'anthropic', 'billing' => 'subscription', 'available' => false, 'cost_class' => 'subscription', 'unavailable_reason' => 'sign in with `claude` first'],
                ['name' => 'codex', 'display_name' => 'Codex', 'provider' => 'openai', 'billing' => 'subscription', 'available' => true, 'cost_class' => 'subscription'],
                ['name' => 'local-coder', 'display_name' => 'Local coder', 'provider' => 'ollama', 'billing' => 'free_local', 'available' => true, 'cost_class' => 'free_local'],
            ],
            'default_model' => 'claude-code',
        ]),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
    ]);

    Livewire::test('pages::client.conversation-create')
        ->assertSet('model', 'codex');
});

it('disables start when no model is ready', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response([
            'models' => [
                ['name' => 'claude-code', 'display_name' => 'Claude Code', 'provider' => 'anthropic', 'billing' => 'subscription', 'available' => false, 'cost_class' => 'subscription', 'unavailable_reason' => 'sign in with `claude` first'],
                ['name' => 'gpt-4o', 'display_name' => 'gpt-4o', 'provider' => 'openai', 'billing' => 'paid_api', 'available' => false, 'cost_class' => 'api', 'unavailable_reason' => 'needs an API key: `cs init --api-key openai`'],
            ],
            'default_model' => null,
        ]),
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
    ]);

    Livewire::test('pages::client.conversation-create')
        ->assertSet('canStart', false)
        ->assertSee('Sign in to a subscription CLI')
        ->call('submit')
        ->assertSet('canStart', false);

    Http::assertNotSent(fn (Request $request) => $request->url() === director('conversations'));
});

it('shows the Director\'s explanation of a failed turn and offers another model when that may help', function () {
    $failed = turnRecord([
        'state' => 'error',
        'reply' => null,
        'detail' => 'the job ended in error',
        'failure' => ['code' => 'auth_expired', 'message' => 'Sign in to this model again.', 'model_switch_may_help' => true, 'retryable' => true],
        'finished_at' => '2026-11-10T10:00:05Z',
    ]);
    fakeThread(conversationRecord(['model' => 'local-coder']), [$failed]);

    $this->get(route('conversations.show', 'con_1'))
        ->assertSee('Sign in to this model again.')
        ->assertDontSeeText('the job ended in error')
        ->assertSee('Try again')
        ->assertSee('Try with another model')
        ->assertSee('Codex · openai · subscription');
    // The turn says it all: no job is fetched to explain it.
    Http::assertNotSent(fn (Request $request) => str_contains($request->url(), 'jobs/'));
});

it('shows the specific cause under the Director\'s sentence when it has one', function () {
    fakeThread(conversationRecord(), [turnRecord(['state' => 'error', 'reply' => null,
        'failure' => ['code' => 'node_config', 'message' => 'Fix the node\'s configuration before retrying.',
            'detail' => 'lead-host: cannot read hosted-lead.json', 'model_switch_may_help' => false, 'retryable' => false]])]);

    $this->get(route('conversations.show', 'con_1'))
        ->assertSee('Fix the node&#039;s configuration before retrying.', false)
        ->assertSee('data-test="turn-failure-detail"', false)
        ->assertSee('lead-host: cannot read hosted-lead.json');
});

it('falls back to the turn\'s own detail for a failure recorded before the Director explained failures', function () {
    fakeThread(conversationRecord(), [turnRecord(['state' => 'error', 'reply' => null, 'detail' => 'the job ended in error'])]);

    $this->get(route('conversations.show', 'con_1'))
        ->assertSee('the job ended in error')
        ->assertSee('Try again')
        ->assertDontSee('Try with another model');
});

it('shows only try again on a failed turn when the Director says another model would not help', function () {
    $failed = turnRecord([
        'state' => 'error',
        'reply' => null,
        'detail' => 'the job ended in error',
        'failure' => ['code' => 'check_failed', 'message' => 'The task\'s check failed.', 'model_switch_may_help' => false, 'retryable' => true],
        'finished_at' => '2026-11-10T10:00:05Z',
    ]);
    fakeThread(conversationRecord(), [$failed]);

    $this->get(route('conversations.show', 'con_1'))
        ->assertSee('The task&#039;s check failed.', false)
        ->assertSee('Try again')
        ->assertDontSee('Try with another model');
});

it('try again posts the same message text as a new turn with a fresh key', function () {
    $failed = turnRecord([
        'state' => 'error',
        'reply' => null,
        'detail' => 'the job ended in error',
        'message' => 'Please retry this.',
        'finished_at' => '2026-11-10T10:00:05Z',
    ]);
    $queued = turnRecord(['turn_id' => 'trn_2', 'seq' => 2, 'message' => 'Please retry this.', 'state' => 'queued', 'job_id' => null, 'run_id' => null]);

    Http::fake([
        director('models') => Http::response(modelsCatalog()),
        director('conversations/con_1/thread') => Http::response(['conversation' => conversationRecord(), 'turns' => [$failed], 'summaries' => []]),
        director('conversations/con_1/turns') => Http::response([
            'turn' => $queued,
            'conversation' => conversationRecord(['queued_turn_ids' => ['trn_2'], 'revision' => 31]),
        ], 201),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1']);
    $page->call('retryTurn', 'trn_1')->assertSee('Queued behind the current reply.');

    $sent = Http::recorded(fn (Request $request) => $request->url() === director('conversations/con_1/turns'));
    expect($sent)->toHaveCount(1);
    [$request] = $sent->first();
    expect($request['message'])->toBe('Please retry this.');
    $usedKey = $request->header('Idempotency-Key')[0] ?? null;
    expect($usedKey)->not->toBeNull()
        ->and($page->get('retryKeys')['trn_1'] ?? null)->not->toBe($usedKey);
});

it('continues a provider-failed turn in a new conversation on another model', function () {
    $failed = turnRecord([
        'state' => 'error',
        'reply' => null,
        'detail' => 'the job ended in error',
        'message' => 'Switch models for this.',
        'failure' => ['code' => 'rate_limit', 'message' => 'Rate limited.', 'model_switch_may_help' => true, 'retryable' => true],
        'finished_at' => '2026-11-10T10:00:05Z',
    ]);

    Http::fake([
        director('models') => Http::response(modelsCatalog()),
        director('conversations/con_1/thread') => Http::response(['conversation' => conversationRecord(['model' => 'local-coder']), 'turns' => [$failed], 'summaries' => []]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['conversation' => conversationRecord(['conversation_id' => 'con_2', 'model' => 'codex'])], 201),
        director('conversations/con_2/turns') => Http::response([
            'turn' => turnRecord(['turn_id' => 'trn_9', 'conversation_id' => 'con_2', 'message' => 'Switch models for this.', 'state' => 'running', 'reply' => null]),
            'conversation' => conversationRecord(['conversation_id' => 'con_2', 'model' => 'codex', 'state' => 'running', 'active_turn_id' => 'trn_9']),
        ], 201),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->set('retryModel.trn_1', 'codex')
        ->call('retryTurnWithModel', 'trn_1')
        ->assertSee('Continued in a new conversation on Codex.')
        ->assertSeeHtml('href="'.route('conversations.show', 'con_2').'"');

    Http::assertSent(function (Request $request) {
        if ($request->method() !== 'POST' || $request->url() !== director('conversations')) {
            return false;
        }

        return $request['spec'] === 'chat' && $request['model'] === 'codex' && $request['node'] === 'laptop' && ! isset($request['files']);
    });
    Http::assertSent(fn (Request $request) => $request->url() === director('conversations/con_2/turns')
        && $request['message'] === 'Switch models for this.');
});

it('does not offer Stop on an idle conversation with nothing queued', function () {
    fakeThread(conversationRecord(), [], []);
    $this->get(route('conversations.show', 'con_1'))->assertDontSee('data-test="stop"', false);
});

it('offers Stop while messages wait', function () {
    fakeThread(conversationRecord(['queued_turn_ids' => ['trn_2']]), [], []);
    $this->get(route('conversations.show', 'con_1'))->assertSee('data-test="stop"', false);
});

it('names an untitled conversation by its model, not its id', function () {
    Http::fake([director('conversations?*') => Http::response(['conversations' => [conversationRecord(['title' => null, 'model' => 'claude-code-sonnet'])]])]);

    $this->get(route('conversations.index'))->assertSeeText('Untitled conversation with claude-code-sonnet')->assertDontSeeText('con_1');
});

it('shows the agent\'s follow-up after its lanes end as a reply with a lane-finished marker, not an empty message', function () {
    Http::preventStrayRequests();
    fakeThread(conversationRecord(), [
        turnRecord(),
        turnRecord(['turn_id' => 'trn_wake', 'seq' => 2, 'origin' => 'wake', 'message' => '', 'posted_by' => 'cred_lead',
            'reply' => 'The **hello** lane finished.', 'detail' => ['jobs' => ['01M7E1360812S1T83MG1BWC3ZF']]]),
    ]);

    $page = $this->get(route('conversations.show', 'con_1'))->assertOk()
        ->assertSeeInOrder(['data-turn="trn_1"', 'data-test="turn-message"', 'General Kenobi.', 'data-turn="trn_wake"', 'data-test="wake-marker"', 'A task lane finished',
            route('lanes.show', '01M7E1360812S1T83MG1BWC3ZF'), 'The <strong>hello</strong> lane finished.'], false);

    expect(substr_count($page->getContent(), 'data-test="turn-message"'))->toBe(1);
});

it('holds and shows only the newest 40 turns of a long thread, and more on request', function () {
    Http::preventStrayRequests();
    $turns = array_map(fn (int $n): array => turnRecord(['turn_id' => "trn_{$n}", 'seq' => $n, 'job_id' => "job_{$n}", 'message' => "message {$n}", 'reply' => "reply {$n}"]), range(1, 50));
    fakeThread(conversationRecord(['turns' => 50]), $turns, [
        summaryRecord(['summary_id' => 'sum_old', 'through_seq' => 5]),
        summaryRecord(['summary_id' => 'sum_new', 'through_seq' => 30]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1']);

    $held = array_keys($page->get('records'));
    expect(array_filter($held, fn (string $key): bool => str_starts_with($key, 'turn:')))->toHaveCount(40)
        ->and($held)->toContain('turn:trn_50', 'turn:trn_11', 'summary:sum_new')
        ->not->toContain('turn:trn_10', 'summary:sum_old');
    $page->assertSee('data-test="show-earlier"', false)->assertSeeText('reply 11')->assertDontSeeText('reply 10');

    $page->call('showEarlier');

    expect(array_filter(array_keys($page->get('records')), fn (string $key): bool => str_starts_with($key, 'turn:')))->toHaveCount(50);
    $page->assertDontSee('data-test="show-earlier"', false)->assertSeeText('reply 1');
});

it('drops a run\'s live text once its turn has finished', function () {
    fakeThread(conversationRecord(), [turnRecord(['state' => 'running', 'reply' => null])], [], [
        director('jobs/job_t1') => Http::response(['job_id' => 'job_t1', 'state' => 'running', 'live_run_ids' => [], 'revision' => 3]),
        director('runs/run_t1/events*') => Http::response(['events' => [], 'confirmed' => 0]),
    ]);
    $event = ['conversation_id' => 'con_1', 'turn_id' => 'trn_1', 'run_id' => 'run_t1', 'seq' => 1, 'type' => 'text', 'text' => 'partial'];

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->call('applyStream', [], [$event]);
    expect($page->get('output'))->toHaveKey('run_t1');

    $page->call('applyStream', [['kind' => 'turn', 'id' => 'trn_1', 'revision' => 99, 'record' => turnRecord(['revision' => 99])]]);

    expect($page->get('output'))->toBe([])->and($page->get('runOwners'))->toBe([]);
});

it('says why a message is not sent when it is too long', function () {
    Http::preventStrayRequests();
    fakeThread(conversationRecord(), [turnRecord()]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->set('message', str_repeat('a', 200001))
        ->call('post')
        ->assertHasErrors('message')
        ->assertSee('data-test="message-error"', false);
});

it('says so when the message does not reach the new conversation after a model switch, and links it instead of starting another', function () {
    $failed = turnRecord(['state' => 'error', 'reply' => null, 'message' => 'Switch models for this.',
        'failure' => ['code' => 'rate_limit', 'message' => 'Rate limited.', 'model_switch_may_help' => true, 'retryable' => true]]);
    Http::fake([
        director('models') => Http::response(modelsCatalog()),
        director('conversations/con_1/thread') => Http::response(['conversation' => conversationRecord(['model' => 'local-coder']), 'turns' => [$failed], 'summaries' => []]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['conversation' => conversationRecord(['conversation_id' => 'con_2', 'model' => 'codex'])], 201),
        director('conversations/con_2/turns') => Http::response(['error' => 'Unprocessable', 'code' => 'invalid_turn', 'detail' => 'message refused'], 422),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->set('retryModel.trn_1', 'codex')
        ->call('retryTurnWithModel', 'trn_1')
        ->assertSet('notice', '')
        ->assertSee('your message did not reach it')
        ->assertSeeHtml('href="'.route('conversations.show', 'con_2').'"');

    expect($page->get('continuedIn')['trn_1']['conversation_id'])->toBe('con_2');
});

it('shows on the new conversation why its first message did not get through', function () {
    Http::preventStrayRequests();
    fakeThread(conversationRecord(), []);

    $this->withSession(['conversation-problem' => 'The conversation started, but your first message did not reach it: timed out. Send it again below.'])
        ->get(route('conversations.show', 'con_1'))
        ->assertOk()
        ->assertSee('your first message did not reach it: timed out');
});

it('carries a refused first message to the new conversation\'s page instead of dropping the reason', function () {
    Http::fake([
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('models') => Http::response(modelsCatalog()),
        director('intents*') => Http::response(['intents' => [], 'jobs' => []]),
        director('projects') => Http::response(['projects' => []]),
        director('objects/*') => Http::response(['stored' => true]),
        director('conversations') => Http::response(['conversation' => conversationRecord()], 201),
        director('conversations/con_1/turns') => Http::response(['error' => 'Unprocessable', 'code' => 'invalid_turn', 'detail' => 'message refused'], 422),
    ]);

    Livewire::test('pages::client.conversation-create')
        ->set('kind', 'chat')
        ->set('message', 'Hello there.')
        ->call('submit')
        ->assertRedirect(route('conversations.show', 'con_1'));

    expect(session('conversation-problem'))->toContain('your first message did not reach it');
});

it('archives with a key: the composer gives way to an Unarchive banner, and unarchiving brings it back', function () {
    fakeThread(conversationRecord(), [turnRecord()], [], [
        director('conversations/con_1/archive') => Http::response(['conversation' => conversationRecord(['archived_at' => '2026-11-15T09:00:00Z', 'revision' => 30])]),
        director('conversations/con_1/unarchive') => Http::response(['conversation' => conversationRecord(['archived_at' => null, 'revision' => 31])]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->assertSee('data-test="composer"', false)->assertSee('data-test="archive"', false);
    $key = $page->get('archiveKey');

    $page->call('archive')
        ->assertDispatched('threads-changed')
        ->assertSee('data-test="archived-banner"', false)->assertDontSee('data-test="composer"', false)
        ->assertSee('data-test="unarchive"', false);
    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('conversations/con_1/archive')
        && $request->hasHeader('Idempotency-Key', $key));

    $page->call('unarchive')->assertSee('data-test="composer"', false)->assertDontSee('data-test="archived-banner"', false);
});

it('deletes with a key and opens the conversation list, which says what was deleted', function () {
    fakeThread(conversationRecord(), [turnRecord()], [], [
        director('conversations/con_1') => Http::response(['deleted' => 'con_1']),
        director('conversations?*') => Http::response(['conversations' => []]),
    ]);

    $page = Livewire::test('pages::client.conversation', ['conversation' => 'con_1']);
    $key = $page->get('deleteKey');
    $page->call('deleteConversation')->assertRedirect(route('conversations.index'))->assertDispatched('threads-changed');

    Http::assertSent(fn (Request $request) => $request->method() === 'DELETE' && $request->url() === director('conversations/con_1')
        && $request->hasHeader('Idempotency-Key', $key));
    $this->get(route('conversations.index'))->assertSee('Deleted “planning chat”.');
});

it('says to stop a busy conversation before deleting it, and stays on it', function () {
    fakeThread(conversationRecord(['state' => 'running']), [turnRecord()], [], [
        director('conversations/con_1') => Http::response(['error' => 'Conflict', 'code' => 'busy', 'detail' => 'a turn is running'], 409),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->call('deleteConversation')
        ->assertNoRedirect()
        ->assertSee('A message is still queued or running. Stop the conversation, then delete it.');
});

it('leaves a conversation deleted elsewhere for the list', function () {
    fakeThread(conversationRecord(), [turnRecord()]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->call('applyStream', [['kind' => 'conversation', 'id' => 'con_1', 'revision' => 99, 'deleted' => true]])
        ->assertRedirect(route('conversations.index'));
});

it('lists archived conversations apart, and moves one between the views as it is archived', function () {
    Http::fake(fn (Request $request) => Http::response(['conversations' => ($request->data()['archived'] ?? null) === '1'
        ? [conversationRecord(['conversation_id' => 'con_old', 'title' => 'put away', 'archived_at' => '2026-11-14T00:00:00Z'])]
        : [conversationRecord(['title' => 'still here'])]]));

    $page = Livewire::test('pages::client.conversations')->assertSee('still here')->assertDontSee('put away');
    $page->call('applyStream', [['kind' => 'conversation', 'id' => 'con_1', 'revision' => 20,
        'record' => conversationRecord(['title' => 'still here', 'archived_at' => '2026-11-15T00:00:00Z', 'revision' => 20])]])
        ->assertDontSee('still here');

    $page->set('archived', true)->assertSee('put away');
    Http::assertSent(fn (Request $request) => str_contains($request->url(), 'archived=1'));

    $page->call('applyStream', [['kind' => 'conversation', 'id' => 'con_old', 'revision' => 30, 'deleted' => true]])
        ->assertDontSee('put away')->assertSee('No archived conversations.');
});

it('shows an approval the conversation\'s attention links, in place of a question to answer', function () {
    fakeThread(conversationRecord(['attention' => attentionRecord([
        'source_kind' => 'action_approval', 'action_approval_id' => 'apr_1', 'question' => 'Pushing leaves the machine.', 'job_id' => 'job_lane',
    ])]), [], [], [
        director('action-approvals/apr_1') => Http::response(['id' => 'apr_1', 'job_id' => 'job_lane', 'status' => 'needs_owner', 'argv' => ['git', 'push', 'origin', 'lane/fix it'],
            'needs' => ['network'], 'reason' => 'Push the lane branch.', 'verdict' => 'no_go', 'review_reason' => 'Pushing leaves the machine.']),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->assertSee("git push origin 'lane/fix it'")
        ->assertSee('Approve')
        ->assertDontSee('Needs your answer');
});

it('says a turn is waiting for its machine or held, instead of waiting for the reply, and polls while it is', function () {
    $this->travelTo(CarbonImmutable::parse('2026-11-24T10:00:00Z'));
    fakeThread(conversationRecord(['state' => 'running', 'active_turn_id' => 'trn_1', 'turns' => 2]), [
        turnRecord(['state' => 'running', 'reply' => null, 'run_id' => null, 'finished_at' => null,
            'waiting' => ['reason' => 'node_not_pulled', 'node' => 'node-a', 'since' => '2026-11-24T09:59:00Z', 'node_last_seen' => '2026-11-24T09:50:00Z']]),
        turnRecord(['turn_id' => 'trn_2', 'seq' => 2, 'state' => 'queued', 'reply' => null, 'job_id' => null, 'run_id' => null, 'context' => null, 'waiting' => ['reason' => 'held', 'detail' => 'over budget']]),
    ], [], [
        director('jobs/job_t1') => Http::response(['job_id' => 'job_t1', 'state' => 'offered', 'live_run_ids' => [], 'conversation_id' => 'con_1', 'revision' => 13]),
    ]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])
        ->assertSee('Waiting for node-a to pick this up (last heard from 10 minutes ago)…')
        ->assertSee('Held: over budget.')
        ->assertDontSee('Waiting for the reply…')
        ->assertSeeHtml('wire:poll.10s="refetch"');
});

it('does not poll a conversation with nothing in progress', function () {
    fakeThread(conversationRecord(['turns' => 1]), [turnRecord()]);

    Livewire::test('pages::client.conversation', ['conversation' => 'con_1'])->assertDontSeeHtml('wire:poll.10s="refetch"');
});
