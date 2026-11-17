<?php

use Illuminate\Support\Facades\Http;

/*
 * Director.Plan step 8: old addresses lead to what replaced them, without touching the Director or a database.
 */
it('leads old addresses to what replaced them', function (string $old, string $new) {
    Http::fake();

    $this->get($old)->assertRedirect($new);
    Http::assertNothingSent();
})->with([
    'settings' => ['/settings', '/connect'],
    'settings profile' => ['/settings/profile', '/connect'],
    'settings teams' => ['/settings/teams/acme', '/connect'],
    'login' => ['/login', '/intents'],
    'register' => ['/register', '/intents'],
    'dashboard' => ['/dashboard', '/intents'],
    'team bots' => ['/acme/bots', '/conversations'],
    'team bot' => ['/acme/bots/7', '/conversations'],
    'work thread' => ['/acme/work/thread-1', '/conversations'],
    'team chat' => ['/acme/chat/9', '/conversations'],
    'cockpit projects' => ['/acme/cockpit/projects', '/projects'],
]);

it('serves no node API and no bot API: nodes talk only to director-rs', function (string $path) {
    $this->getJson($path)->assertNotFound();
})->with(['/api/v1/nodes/me/jobs', '/api/v1/bots/me', '/broadcasting/auth']);
