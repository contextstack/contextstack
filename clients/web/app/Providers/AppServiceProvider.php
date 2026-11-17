<?php

namespace App\Providers;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use Carbon\CarbonImmutable;
use Illuminate\Contracts\Foundation\Application;
use Illuminate\Support\Facades\Date;
use Illuminate\Support\ServiceProvider;

class AppServiceProvider extends ServiceProvider
{
    /**
     * Register any application services.
     */
    public function register(): void
    {
        // client-web: this device's connection and the Director's Client API (config/client.php).
        $this->app->bind(ConnectionStore::class, fn (): ConnectionStore => ConnectionStore::fromConfig());
        $this->app->bind(DirectorClient::class, fn (Application $application): DirectorClient => new DirectorClient(
            $application->make(ConnectionStore::class)->resolve(),
            (int) config('client.timeout'),
        ));
    }

    /**
     * Bootstrap any application services.
     */
    public function boot(): void
    {
        Date::use(CarbonImmutable::class);
    }
}
