<?php

use Illuminate\Support\Facades\File;
use Illuminate\Support\Facades\Http;
use Tests\TestCase;

/*
|--------------------------------------------------------------------------
| Test Case
|--------------------------------------------------------------------------
|
| The closure you provide to your test functions is always bound to a specific PHPUnit test
| case class. By default, that class is "PHPUnit\Framework\TestCase". Of course, you may
| need to change it using the "pest()" function to bind different classes or traits.
|
*/

// client-web keeps no database: there is nothing to refresh between tests.
pest()->extend(TestCase::class)->in('Feature');

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

function something()
{
    // ..
}

/**
 * client-web tests: a temp settings file and a local credential holding $token, against the fake Director at
 * http://director.test (faked at the HTTP boundary with Http::fake). Returns the temp directory.
 */
function connectClientWeb(string $token = 'csl_local_test'): string
{
    $directory = sys_get_temp_dir().'/client-web-test-'.bin2hex(random_bytes(6));
    mkdir($directory, 0700, true);
    file_put_contents($directory.'/local-credential', $token."\n");
    test()->beforeApplicationDestroyed(fn () => File::deleteDirectory($directory));

    // A Director without the shared chief-of-staff slot unless a test gives it one (chiefOfStaffOnDirector).
    $GLOBALS['cs_test_chief_of_staff'] = null;
    Http::fake(fn ($request) => $request->url() === director('chief-of-staff') && $request->method() === 'GET'
        ? Http::response(...($GLOBALS['cs_test_chief_of_staff'] ?? [['error' => 'Not Found', 'code' => 'not_found'], 404]))
        : null);

    config([
        'client.director_url' => 'http://director.test',
        'client.local_credential' => $directory.'/local-credential',
        'client.settings_path' => $directory.'/settings/connection.json',
    ]);

    return $directory;
}

/**
 * The fake Director holds the chief-of-staff slot (`GET chief-of-staff`) as given.
 *
 * @param  array<string, mixed>  $slot
 */
function chiefOfStaffOnDirector(array $slot): void
{
    $GLOBALS['cs_test_chief_of_staff'] = [['chief_of_staff' => [...['enabled' => false, 'name' => 'Chief of staff', 'conversation_id' => null, 'revision' => 1], ...$slot]], 200];
}

/**
 * A Client API URL on the fake Director.
 */
function director(string $path): string
{
    return 'http://director.test/api/v1/client/'.ltrim($path, '/');
}
