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
    // `GET me` (Identity) answers as a Director from before teams (an owner, no capabilities) unless a test gives it
    // other answers with meOnDirector. Stubs for `me` given to Http::fake later never match: this one comes first.
    $GLOBALS['cs_test_me'] = [];
    // `GET advertised-url` answers unset unless a test sets one with advertisedUrlOnDirector.
    $GLOBALS['cs_test_advertised_url'] = null;
    Http::fake(function ($request) {
        if (str_ends_with($request->url(), '/api/v1/client/advertised-url') && $request->method() === 'GET') {
            return Http::response(['url' => $GLOBALS['cs_test_advertised_url']]);
        }
        if (str_ends_with($request->url(), '/api/v1/client/chief-of-staff') && $request->method() === 'GET') {
            return Http::response(...($GLOBALS['cs_test_chief_of_staff'] ?? [['error' => 'Not Found', 'code' => 'not_found'], 404]));
        }
        if (str_ends_with($request->url(), '/api/v1/client/me')) {
            $answers = $GLOBALS['cs_test_me'];
            $answer = count($answers) > 1 ? array_shift($GLOBALS['cs_test_me']) : ($answers[0] ?? [['credential_id' => 'cred_local', 'owner' => true], 200]);

            return $answer === ['unreachable'] ? Http::failedConnection() : Http::response(...$answer);
        }

        return null;
    });
    // Nothing unfaked leaves the test: a request no stub answers fails instead of reaching a real host.
    Http::preventStrayRequests();

    config([
        'client.director_url' => 'http://director.test',
        'client.local_credential' => $directory.'/local-credential',
        'client.settings_path' => $directory.'/settings/connection.json',
    ]);

    return $directory;
}

/**
 * The fake Director answers `GET advertised-url` with this URL (null: unset).
 */
function advertisedUrlOnDirector(?string $url): void
{
    $GLOBALS['cs_test_advertised_url'] = $url;
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

/**
 * The capabilities a teams Director advertises for each role (director/src/policy.rs CAPABILITIES).
 *
 * @return list<string>
 */
function capabilitiesOf(string $role): array
{
    $viewer = ['read_work', 'manage_own_devices'];
    $member = [...$viewer, 'start_work', 'control_work', 'decide_run', 'edit_backlog', 'edit_decision_log', 'sponsor_lead'];
    $admin = [...$member, 'approve_action', 'read_source_budgets', 'manage_director_budgets', 'manage_members', 'manage_member_devices', 'edit_projects', 'manage_schedules'];
    $owner = [...$admin, 'manage_source_budgets', 'manage_org_settings', 'land'];

    return match ($role) {
        'owner' => $owner,
        'admin' => $admin,
        'member' => $member,
        default => $viewer,
    };
}

/**
 * A teams Director's `GET me` for a credential acting for $memberId in $role.
 *
 * @return array<string, mixed>
 */
function meAs(string $role, string $memberId = 'mem_me', string $name = 'Robin'): array
{
    return ['credential_id' => 'cred_this', 'owner' => $role === 'owner', 'member' => ['id' => $memberId, 'display_name' => $name],
        'role' => $role, 'capabilities' => capabilitiesOf($role)];
}

/**
 * The fake Director answers `GET me` with these, in order, the last one from then on. Each is a `me` body (200),
 * a [body, status] pair for a refusal, or ['unreachable'] for a Director that cannot be reached.
 *
 * @param  array<string|int, mixed>  ...$answers
 */
function meOnDirector(array ...$answers): void
{
    $GLOBALS['cs_test_me'] = array_map(fn (array $answer): array => $answer === ['unreachable'] || (array_is_list($answer) && count($answer) === 2 && is_int($answer[1])) ? $answer : [$answer, 200], $answers);
}

/**
 * Answers for what a page reads on the side (the frame, pickers, an approvals list), registered after a test's own
 * stubs so those win: the first matching stub answers. Call it after the test's Http::fake. With stray requests
 * prevented, a read neither the test nor these answer fails the test instead of reaching a real host.
 */
function fakeDirectorBackground(): void
{
    Http::fake([
        '*/api/v1/client/lead-node' => Http::response(['error' => 'Not Found', 'code' => 'not_found', 'detail' => 'no such route'], 404),
        '*/api/v1/client/nodes' => Http::response(['nodes' => [], 'stale_after_seconds' => 300]),
        '*/api/v1/client/projects' => Http::response(['projects' => []]),
        '*/api/v1/client/projects/*/heartbeat' => Http::response(['project_id' => 'prj_1', 'interval_seconds' => null, 'conversation_id' => null]),
        '*/api/v1/client/action-approvals*' => Http::response(['action_approvals' => []]),
        '*/api/v1/client/members' => Http::response(['members' => []]),
        '*/api/v1/client/members/directory' => Http::response(['error' => 'Not Found', 'code' => 'not_found', 'detail' => 'no such route'], 404),
        '*/api/v1/client/models' => Http::response(['models' => [], 'default_model' => null]),
    ]);
}
