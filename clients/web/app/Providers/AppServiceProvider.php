<?php

namespace App\Providers;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Identity;
use App\Client\MemberNames;
use App\Client\RecordActions;
use App\Livewire\DirectorPage;
use Carbon\CarbonImmutable;
use Illuminate\Contracts\Foundation\Application;
use Illuminate\Support\Facades\Blade;
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
        // Who this device is and member names: asked once per request at most.
        $this->app->scoped(Identity::class);
        $this->app->scoped(MemberNames::class);
    }

    /**
     * Bootstrap any application services.
     */
    public function boot(): void
    {
        Date::use(CarbonImmutable::class);

        // @capable('start_work') … @endcapable: show a control only when the Director advertises the capability
        // (Identity::can; a Director from before teams advertises none, and everything shows as before).
        // A DirectorPage learns which action a request runs, so a refused write (not a read) refreshes the page.
        \Livewire\on('call', function (mixed $component, string $method): void {
            if ($component instanceof DirectorPage) {
                $component->calling($method);
            }
        });

        Blade::if('capable', fn (string $capability): bool => app(Identity::class)->can($capability));

        // @allowed($record, 'cancel', 'control_work') … @endallowed: the record's own `actions` decide when it has
        // them; otherwise the capability does, as @capable (RecordActions).
        Blade::if('allowed', fn (mixed $record, string $action, string $capability): bool => RecordActions::allowsOr($record, $action, $capability));
    }
}
