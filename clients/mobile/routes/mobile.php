<?php

use App\Http\Middleware\RequireDirectorConnection;
use App\Http\Middleware\RequireUnlock;
use App\NativeComponents\Connect;
use App\NativeComponents\Devices;
use App\NativeComponents\Locked;
use App\NativeComponents\ProjectBacklog;
use App\NativeComponents\Projects;
use App\NativeComponents\Settings;
use App\NativeComponents\Waiting;
use Illuminate\Support\Facades\Route;

Route::native('/connect', Connect::class);
Route::native('/pair', Connect::class);

Route::middleware(RequireDirectorConnection::class)->group(function (): void {
    Route::native('/locked', Locked::class);
});

Route::middleware([RequireDirectorConnection::class, RequireUnlock::class])->group(function (): void {
    Route::native('/', Waiting::class);
    Route::native('/projects', Projects::class);
    Route::native('/projects/{id}', ProjectBacklog::class);
    Route::native('/settings', Settings::class);
    Route::native('/devices', Devices::class);
});
