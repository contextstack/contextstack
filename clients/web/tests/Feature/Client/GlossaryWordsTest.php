<?php

use Illuminate\Support\Facades\Http;

/**
 * Visible copy on task pages must not use the retired word "intent" (see GLOSSARY.md).
 */
function visibleTextWithoutAttributes(string $html): string
{
    $withoutAttributes = preg_replace(
        '/\s(?:[a-zA-Z_:.-]+)\s*=\s*(?:"[^"]*"|\'[^\']*\'|\{[^}]*\})/',
        '',
        $html,
    ) ?? $html;

    return html_entity_decode(strip_tags($withoutAttributes), ENT_QUOTES | ENT_HTML5, 'UTF-8');
}

it('does not show the word intent on the tasks index or new-task page', function () {
    connectClientWeb();
    Http::fake([
        director('intents?*') => Http::response(['intents' => [], 'jobs' => []]),
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('projects') => Http::response(['projects' => []]),
    ]);

    foreach ([route('intents.index'), route('intents.create')] as $url) {
        $html = $this->get($url)->assertOk()->getContent();
        $visible = visibleTextWithoutAttributes($html);
        expect($visible)->not->toMatch('/intent/i');
    }
});

it('calls a node a machine in visible copy', function () {
    connectClientWeb();
    $seen = '2026-11-13T10:00:00Z';
    Http::fake([
        director('intents') => Http::response([
            'intents' => [['intent_id' => 'int_1', 'title' => 'say hello', 'task' => 'tasks/hello', 'node' => 'laptop', 'meta' => [], 'created_at' => $seen, 'revision' => 1]],
            'jobs' => [['job_id' => 'job_1', 'intent_id' => 'int_1', 'state' => 'done', 'created_at' => $seen, 'cancel' => null, 'revision' => 2]],
        ]),
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null, 'last_seen_at' => $seen, 'protocol' => 2, 'open_jobs' => 0,
            'status' => ['active' => [], 'reported_at' => $seen]]], 'stale_after_seconds' => 300]),
        director('projects') => Http::response(['projects' => []]),
        director('conversations?*') => Http::response(['conversations' => []]),
        director('usage*') => Http::response(['by' => 'node', 'period' => 'day', 'from' => $seen, 'to' => $seen,
            'groups' => [['node' => 'laptop', 'runs' => 1, 'charged_usd' => 0, 'kinds' => []]], 'total' => ['runs' => 1, 'charged_usd' => 0, 'kinds' => []]]),
        director('*') => Http::response([]),
    ]);

    $pages = [route('intents.index'), route('intents.create'), route('lanes.index'), route('lanes.create'), route('fleet'),
        route('conversations.index'), route('conversations.create'), route('usage', ['by' => 'node', 'node' => 'laptop'])];
    foreach ($pages as $url) {
        $visible = visibleTextWithoutAttributes($this->get($url)->assertOk()->getContent());
        expect($visible)->not->toMatch('/\bnodes?\b/i', $url);
    }
});

it('never says lead in visible copy while the chief-of-staff slot is off', function () {
    connectClientWeb();
    $conversation = ['conversation_id' => 'con_1', 'title' => 'widget work', 'node' => 'laptop', 'model' => 'claude-code-sonnet', 'spec' => 'spec.yaml',
        'state' => 'idle', 'attention' => ['question' => 'Which branch?', 'source_kind' => 'lead', 'set_at' => '2026-11-14T00:00:00Z'],
        'queued_turn_ids' => [], 'turns' => 0, 'created_at' => '2026-11-14T00:00:00Z', 'revision' => 1, 'lead' => true];
    Http::fake([
        director('conversations?*') => Http::response(['conversations' => [$conversation]]),
        director('conversations/con_1/thread') => Http::response(['conversation' => $conversation, 'turns' => [], 'summaries' => []]),
        director('models') => Http::response(['models' => [['name' => 'claude-code-sonnet', 'lead' => true, 'available' => true]], 'default_model' => 'claude-code-sonnet']),
        director('nodes') => Http::response(['nodes' => [['name' => 'laptop', 'revoked_at' => null]]]),
        director('*') => Http::response([]),
    ]);

    foreach ([route('conversations.index'), route('conversations.create'), route('conversations.show', 'con_1')] as $url) {
        $visible = visibleTextWithoutAttributes($this->get($url)->assertOk()->getContent());
        expect($visible)->not->toMatch('/\\blead\\b/i', $url);
    }
});
