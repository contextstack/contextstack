<?php

namespace App\Providers;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\IdentityStore;
use App\Native\AppLock;
use Carbon\CarbonImmutable;
use Illuminate\Contracts\Foundation\Application;
use Illuminate\Support\Facades\Date;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Event;
use Illuminate\Support\ServiceProvider;
use Illuminate\Validation\Rules\Password;
use Native\Mobile\Events\App\AppForegrounded;

class AppServiceProvider extends ServiceProvider
{
    /**
     * Register any application services.
     */
    public function register(): void
    {
        $this->app->bind(ConnectionStore::class, fn (): ConnectionStore => ConnectionStore::fromConfig());
        // A singleton on purpose: the runtime keeps it for the whole launch, so "unlocked" lasts until the app has been
        // away long enough or is launched again.
        $this->app->singleton(AppLock::class, fn (): AppLock => AppLock::fromConfig());
        // Who this credential acts for (`GET me`), kept for the launch and read again when it may have changed.
        $this->app->singleton(IdentityStore::class);
        $this->app->bind(DirectorClient::class, function (Application $app): DirectorClient {
            $timeout = config('client.timeout');

            return new DirectorClient($app->make(ConnectionStore::class)->resolve(), is_int($timeout) ? $timeout : 15);
        });
    }

    /**
     * Bootstrap any application services.
     */
    public function boot(): void
    {
        $this->configureDefaults();

        Event::listen(AppForegrounded::class, function (AppForegrounded $event): void {
            $this->app->make(AppLock::class)->returned($event->secondsAway);
            // A role or membership may have changed while the app was away.
            $this->app->make(IdentityStore::class)->forget();
        });
    }

    /**
     * Configure default behaviors for production-ready applications.
     */
    protected function configureDefaults(): void
    {
        Date::use(CarbonImmutable::class);

        DB::prohibitDestructiveCommands(
            app()->isProduction(),
        );

        Password::defaults(fn (): ?Password => app()->isProduction()
            ? Password::min(12)
                ->mixedCase()
                ->letters()
                ->numbers()
                ->symbols()
                ->uncompromised()
            : null,
        );
    }
}
