<?php

use App\Http\Controllers\Client\EventStreamController;
use App\Http\Controllers\Client\EvidenceController;
use App\Http\Controllers\Client\HomeController;
use App\Http\Controllers\NativeShellController;
use App\Http\Middleware\RequireDirectorConnection;
use Illuminate\Support\Facades\Route;

/*
 * client-web: a client of the Director (tests/director-spec/CLIENT-API.md). No login: access is this device's Director
 * credential (a paired device token or the local credential), and every page reads the Director, never a database.
 */
Route::livewire('connect', 'pages::client.connect')->name('connect');
Route::livewire('desktop/offline', 'pages::client.desktop-gate')->name('desktop.gate');

Route::post('native/open-external', [NativeShellController::class, 'openExternal'])->name('native.open-external');

Route::middleware(RequireDirectorConnection::class)->group(function (): void {
    Route::get('/', HomeController::class)->name('home');
    Route::livewire('intents', 'pages::client.intents')->name('intents.index');
    Route::livewire('intents/new', 'pages::client.intent-create')->name('intents.create');
    Route::livewire('intents/{intent}', 'pages::client.intent')->name('intents.show');
    Route::livewire('lanes', 'pages::client.lanes')->name('lanes.index');
    Route::livewire('lanes/new', 'pages::client.lane-create')->name('lanes.create');
    Route::livewire('lanes/{job}', 'pages::client.lane')->name('lanes.show');
    Route::livewire('runs/{run}', 'pages::client.run')->name('runs.show');
    Route::get('runs/{run}/files/{path}', [EvidenceController::class, 'run'])->where('path', '.+')->name('runs.files');
    Route::get('jobs/{job}/files/{path}', [EvidenceController::class, 'job'])->where('path', '.+')->name('jobs.files');
    Route::livewire('fleet', 'pages::client.fleet')->name('fleet');
    Route::livewire('conversations', 'pages::client.conversations')->name('conversations.index');
    Route::livewire('conversations/new', 'pages::client.conversation-create')->name('conversations.create');
    Route::livewire('conversations/{conversation}', 'pages::client.conversation')->name('conversations.show');
    Route::livewire('projects', 'pages::client.projects')->name('projects.index');
    Route::livewire('projects/{project}', 'pages::client.project')->name('projects.show');
    Route::livewire('projects/{project}/decisions', 'pages::client.decision-log')->name('decisions.index');
    Route::livewire('projects/{project}/decisions/{entry}', 'pages::client.decision')->name('decisions.show');
    Route::livewire('projects/{project}/backlog', 'pages::client.backlog')->name('backlog.index');
    Route::livewire('projects/{project}/backlog/{item}', 'pages::client.backlog-item')->name('backlog.show');
    Route::livewire('approvals', 'pages::client.approvals')->name('approvals');
    Route::livewire('devices', 'pages::client.devices')->name('devices');
    Route::livewire('usage', 'pages::client.usage')->name('usage');
    Route::livewire('sources', 'pages::client.sources')->name('sources');
    Route::get('stream', EventStreamController::class)->name('stream');
});

/*
 * Old addresses (Director.Plan step 8). Settings are this device's connection now. There is no login (outline
 * decision 7), so the account pages lead home. Bots arrive later as a Director layer and teams with accounts
 * (decisions 5 and 10); their team-scoped pages are gone. Old record ids name nothing in the Director, so each old
 * address leads to the matching list.
 */
Route::redirect('settings', '/connect');
Route::redirect('settings/{page}', '/connect')->where('page', '.*');
foreach (['login', 'register', 'logout', 'dashboard', 'forgot-password', 'two-factor-challenge', 'user/confirm-password'] as $retired) {
    Route::redirect($retired, '/intents');
}
Route::prefix('{team}')->group(function (): void {
    Route::redirect('chat', '/conversations');
    Route::redirect('chat/{conversation}', '/conversations');
    Route::redirect('cockpit/chat', '/conversations');
    Route::redirect('cockpit/chat/{conversation}', '/conversations');
    Route::redirect('cockpit/projects', '/projects');
    Route::redirect('projects', '/projects');
    Route::redirect('projects/{project}', '/projects');
    Route::redirect('work/{thread}', '/conversations');
    Route::redirect('bots', '/conversations');
    Route::redirect('bots/{bot}', '/conversations');
    Route::redirect('cockpit/bots', '/conversations');
});
