<?php

use App\Client\ConnectionStore;
use Illuminate\Support\Facades\Http;
use Native\Mobile\Testing\Native;
use Tests\TestCase;

/*
|--------------------------------------------------------------------------
| Test Case
|--------------------------------------------------------------------------
|
| The closure you provide to your test functions is always bound to a specific PHPUnit test
| case class. By default, that class is "PHPUnit\Framework\TestCase". Of course, you may
| need to change it using the "pest()" function to bind a different classes or traits.
|
*/

pest()->extend(TestCase::class)
 // ->use(Illuminate\Foundation\Testing\RefreshDatabase::class)
    ->in('Feature');

/*
|--------------------------------------------------------------------------
| Expectations
|--------------------------------------------------------------------------
|
| When you're writing tests, you often need to check that values meet certain conditions. The
| "expect()" function gives you access to a set of "expectations" methods that you can use
| to assert different things. Of course, you may extend the Expectation API at any time.
|
*/

expect()->extend('toBeOne', function () {
    return $this->toBe(1);
});

/*
|--------------------------------------------------------------------------
| Functions
|--------------------------------------------------------------------------
|
| While Pest is very powerful out-of-the-box, you may have some testing code specific to your
| project that you don't want to repeat in every file. Here you can also expose helpers as
| global functions to help you to reduce the number of lines of code in your test files.
|
*/

const DirectorUrl = 'http://192.0.2.1:8780';

/**
 * A temporary connection file and lock file, no stray HTTP requests, and the fake bridge with secure storage.
 */
function isolateDevice(): void
{
    $directory = sys_get_temp_dir().'/cs-mobile-test-'.bin2hex(random_bytes(4));
    config(['client.settings_path' => $directory.'/connection.json', 'client.lock_path' => $directory.'/lock.json', 'client.director_url' => DirectorUrl]);
    Http::preventStrayRequests();
    Native::fakeBridge()->withSecureStorage();
}

/**
 * Pair this device. Register a screen's own fakes (and any `me`) before calling it: the first matching fake wins.
 */
function pairDevice(): void
{
    app(ConnectionStore::class)->saveDevice(DirectorUrl, 'csd_test_token', 'cred_ipad', 'iPad');
    Http::fake([
        DirectorUrl.'/api/v1/client/projects' => Http::response(['projects' => []]),
        // A Director from before teams: no member, role or capabilities, so every action is offered.
        DirectorUrl.'/api/v1/client/me' => Http::response(['credential_id' => 'cred_ipad', 'owner' => true]),
        // A Director without the member directory yet.
        DirectorUrl.'/api/v1/client/members/directory' => Http::response(['error' => 'Not found', 'code' => 'not_found'], 404),
    ]);
}
