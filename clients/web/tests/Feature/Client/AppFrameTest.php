<?php

use Illuminate\Support\Facades\Http;

beforeEach(fn () => connectClientWeb());

it('frames every page with the icon rail and the collapsible thread panel', function () {
    Http::fake([director('nodes') => Http::response(['nodes' => []])]);

    $this->get(route('fleet'))->assertOk()
        ->assertSee('data-test="nav-rail"', false)
        ->assertSee('data-test="sidebar-panel"', false)
        ->assertSee('data-test="sidebar-hide"', false)
        ->assertSee('data-test="sidebar-show"', false)
        ->assertSee(route('lanes.index'), false);
});

it('leaves the theme to the system unless the settings page picks one', function () {
    Http::fake([director('me') => Http::response(['credential_id' => 'cred_local', 'owner' => true])]);

    $this->get(route('fleet'))->assertDontSee('<html lang="en" class="dark"', false);
    $this->get(route('connect'))->assertOk()
        ->assertSee('data-test="appearance"', false)
        ->assertSee('$flux.appearance', false)
        ->assertSeeInOrder(['System', 'Light', 'Dark']);
});

it('puts a thread\'s details in its side panel, with the lanes first on a lead', function () {
    $conversation = ['conversation_id' => 'con_lead', 'title' => 'my lead', 'node' => 'workstation', 'model' => 'claude-code-sonnet', 'spec' => 'spec.yaml',
        'state' => 'idle', 'attention' => null, 'queued_turn_ids' => [], 'turns' => 0, 'created_at' => '2026-11-14T10:00:00Z', 'revision' => 1, 'lead' => true];
    Http::fake([
        director('models') => Http::response(['models' => [], 'default_model' => null]),
        director('conversations/con_lead/thread') => Http::response(['conversation' => $conversation, 'turns' => [], 'summaries' => []]),
        director('intents*') => Http::response(['intents' => []]),
        director('jobs*') => Http::response(['jobs' => []]),
    ]);

    $this->get(route('conversations.show', 'con_lead'))->assertOk()
        ->assertSee('data-full-bleed', false)
        ->assertSee('data-test="side-panel"', false)
        ->assertSee("tab: 'lanes'", false)
        ->assertSeeInOrder(['data-test="lanes-pane"', 'data-test="conversation-details"', 'spec.yaml', 'workstation'], false)
        ->assertSee('What should we work on?');
});

it('writes messages in Flux\'s editor, which sends Markdown through the same post', function () {
    $conversation = ['conversation_id' => 'con_chat', 'title' => 'chat', 'node' => 'workstation', 'model' => 'local-coder', 'spec' => 'spec.yaml',
        'state' => 'idle', 'attention' => null, 'queued_turn_ids' => [], 'turns' => 0, 'created_at' => '2026-11-14T10:00:00Z', 'revision' => 1, 'lead' => false];
    Http::fake([
        director('models') => Http::response(['models' => [], 'default_model' => null]),
        director('conversations/con_chat/thread') => Http::response(['conversation' => $conversation, 'turns' => [], 'summaries' => []]),
    ]);

    $this->get(route('conversations.show', 'con_chat'))->assertOk()
        ->assertSee('data-cs-composer', false)
        ->assertSee('x-data="composer"', false)
        ->assertSeeInOrder(['<ui-editor', 'x-on:keydown.capture="keydown($event)"', '<ui-editor-content>'], false)
        ->assertSee('x-on:click="send()"', false)
        ->assertDontSee('<textarea id="composer-message"', false);
});

it('writes the first message of a new conversation in the same editor, kept in the form\'s message field', function () {
    Livewire::test('pages::client.conversation-create')
        ->assertSeeHtml('x-data="composer({ form: true })"')
        ->assertSeeHtmlInOrder(['data-test="first-message"', '<ui-editor', 'x-on:keydown.capture="keydown($event)"', '<ui-editor-content>'])
        ->assertDontSeeHtml('<textarea');
});
