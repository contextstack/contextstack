<?php

use App\Http\Middleware\RequireDirectorConnection;
use App\Http\Middleware\RequireUnlock;
use App\NativeComponents\BrowserScreenshot;
use App\NativeComponents\Budgets;
use App\NativeComponents\Connect;
use App\NativeComponents\Conversation;
use App\NativeComponents\Conversations;
use App\NativeComponents\Devices;
use App\NativeComponents\Invite;
use App\NativeComponents\Lane;
use App\NativeComponents\Lanes;
use App\NativeComponents\Locked;
use App\NativeComponents\Machines;
use App\NativeComponents\Members;
use App\NativeComponents\NewConversation;
use App\NativeComponents\NewLane;
use App\NativeComponents\ProjectBacklog;
use App\NativeComponents\Projects;
use App\NativeComponents\Settings;
use App\NativeComponents\Usage;
use App\NativeComponents\Waiting;
use Illuminate\Support\Facades\Route;

// Pairing is open to an unpaired device; with the app lock on, replacing a pairing needs the device unlock first.
Route::middleware(RequireUnlock::class)->group(function (): void {
    Route::native('/connect', Connect::class);
    Route::native('/pair', Connect::class);
    Route::native('/invite', Invite::class);
});

Route::middleware(RequireDirectorConnection::class)->group(function (): void {
    Route::native('/locked', Locked::class);
});

Route::middleware([RequireDirectorConnection::class, RequireUnlock::class])->group(function (): void {
    Route::native('/', Waiting::class);
    Route::native('/lanes', Lanes::class);
    Route::native('/lanes/new', NewLane::class);
    Route::native('/lanes/{id}', Lane::class);
    Route::native('/projects', Projects::class);
    Route::native('/projects/{id}', ProjectBacklog::class);
    Route::native('/conversations', Conversations::class);
    Route::native('/conversations/new', NewConversation::class);
    Route::native('/conversations/{id}', Conversation::class);
    Route::native('/settings', Settings::class);
    Route::native('/devices', Devices::class);
    Route::native('/members', Members::class);
    Route::native('/machines', Machines::class);
    Route::native('/usage', Usage::class);
    Route::native('/budgets', Budgets::class);
    Route::native('/browser-screenshots/{id}', BrowserScreenshot::class);
});
