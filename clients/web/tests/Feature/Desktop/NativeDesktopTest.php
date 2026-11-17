<?php

use App\Desktop\ApplicationMenu;
use App\Providers\NativeAppServiceProvider;
use Illuminate\Foundation\Http\Middleware\ValidateCsrfToken;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Native\Desktop\Facades\Shell;
use Native\Desktop\Facades\Window;
use Native\Desktop\Fakes\WindowManagerFake;
use Native\Desktop\Windows\Window as NativeWindow;

it('opens one ContextStack window at the configured size when the native app boots', function () {
    Http::fake(['http://localhost:4000/api/*' => Http::response([])]);

    config([
        'nativephp-internal.api_url' => 'http://localhost:4000/api/',
        'nativephp-internal.secret' => 'test-secret',
    ]);

    $main = new NativeWindow('main');

    /** @var WindowManagerFake $windows */
    $windows = Window::fake()->alwaysReturnWindows([$main]);

    app(NativeAppServiceProvider::class)->boot();

    $windows->assertOpenedCount(1);
    $windows->assertOpened('main');

    $options = $main->toArray();

    expect($options['title'])->toBe('ContextStack')
        ->and($options['width'])->toBe(1280)
        ->and($options['height'])->toBe(860)
        ->and($options['minWidth'])->toBe(900)
        ->and($options['minHeight'])->toBe(600)
        ->and($options['rememberState'])->toBeTrue();
});

it('exposes php ini directives from the native app provider', function () {
    expect(app(NativeAppServiceProvider::class)->phpIni())
        ->toBe(['memory_limit' => '512M']);
});

it('configures the ContextStack native app identity', function () {
    expect(config('nativephp.app_id'))->toBe('dev.contextstack.desktop')
        ->and(config('nativephp.version'))->toBe('0.1.0')
        ->and(config('nativephp.description'))->toBe('Delegate coding work to AI agents on your own machines')
        ->and(config('nativephp.website'))->toBe('https://github.com/contextstack/monorepo')
        ->and(config('nativephp.provider'))->toBe(NativeAppServiceProvider::class);
});

it('does not ship Apple or updater secrets in the committed nativephp config', function () {
    $source = file_get_contents(base_path('config/nativephp.php'));

    expect($source)->not->toBeFalse();

    $envOnlySecrets = [
        'GITHUB_TOKEN',
        'GITHUB_AUTOUPDATE_TOKEN',
        'AWS_SECRET_ACCESS_KEY',
        'DO_SPACES_SECRET_ACCESS_KEY',
    ];

    foreach ($envOnlySecrets as $key) {
        expect($source)->toMatch('/env\(\''.$key.'\'\)/')
            ->and($source)->not->toMatch('/env\(\''.$key.'\',\s*[\'"][^\'"]+[\'"]\)/');
    }

    foreach (['NATIVEPHP_APPLE_ID', 'NATIVEPHP_APPLE_ID_PASS', 'NATIVEPHP_APPLE_TEAM_ID'] as $key) {
        expect($source)->not->toMatch('/env\(\''.$key.'\'/');
    }
});

it('defines the ContextStack application menu', function () {
    $items = collect(ApplicationMenu::items())
        ->map(fn ($item) => $item->toArray())
        ->all();

    expect($items)->toHaveCount(5)
        ->and($items[0]['label'])->toBe('ContextStack')
        ->and(collect($items[0]['submenu'])->pluck('role'))->toContain('about', 'hide', 'quit')
        ->and($items[1]['role'])->toBe('editMenu')
        ->and($items[2]['label'])->toBe('View')
        ->and(collect($items[2]['submenu'])->pluck('role'))->toContain('reload', 'togglefullscreen', 'toggleDevTools')
        ->and($items[3]['label'])->toBe('Window')
        ->and(collect($items[3]['submenu'])->pluck('role'))->toContain('minimize', 'zoom')
        ->and($items[4]['label'])->toBe('Help');

    foreach ([0, 2, 3, 4] as $menu) {
        expect($items[$menu])->not->toHaveKey('type', 'a typed top-level item is not a submenu to Electron');
    }

    $helpLinks = collect($items[4]['submenu']);

    expect($helpLinks->pluck('url'))->toContain(ApplicationMenu::GETTING_STARTED_URL, ApplicationMenu::KNOWN_ISSUES_URL)
        ->and($helpLinks->every(fn (array $link) => $link['openInBrowser'] === true))->toBeTrue();
});

it('omits developer tools from the view menu in production', function () {
    app()->detectEnvironment(fn () => 'production');

    $view = collect(ApplicationMenu::items())
        ->map(fn ($item) => $item->toArray())
        ->firstWhere('label', 'View');

    $roles = collect($view['submenu'])->pluck('role');

    expect($roles)->not->toContain('toggleDevTools');
});

it('registers the application menu when the native app boots', function () {
    Http::fake(['http://localhost:4000/api/*' => Http::response([])]);

    config([
        'nativephp-internal.api_url' => 'http://localhost:4000/api/',
        'nativephp-internal.secret' => 'test-secret',
    ]);

    $main = new NativeWindow('main');

    /** @var WindowManagerFake $windows */
    $windows = Window::fake()->alwaysReturnWindows([$main]);

    app(NativeAppServiceProvider::class)->boot();

    $windows->assertOpenedCount(1);

    Http::assertSent(function (Request $request) {
        if (! str_ends_with($request->url(), '/menu')) {
            return false;
        }

        $labels = collect($request['items'])->pluck('label');

        return $labels->contains('ContextStack')
            && $labels->contains('View')
            && $labels->contains('Help');
    });
});

it('refuses to open links when not running as the desktop app', function () {
    config(['nativephp-internal.running' => false]);
    $shell = Shell::fake();

    $this->postJson(route('native.open-external'), ['url' => 'https://example.com'])->assertNotFound();

    expect($shell->openExternalCalls)->toBe([]);
});

it('opens only http, https and mailto links in the default browser', function () {
    config(['nativephp-internal.running' => true]);
    $shell = Shell::fake();

    $this->withoutMiddleware(ValidateCsrfToken::class);
    $this->postJson(route('native.open-external'), ['url' => 'https://example.com/a?b=1&c=2'])->assertNoContent();
    $this->postJson(route('native.open-external'), ['url' => 'mailto:someone@example.com'])->assertNoContent();
    foreach (['file:///etc/passwd', 'vscode://file/etc/hosts', 'javascript:alert(1)', 'ssh://host', 'not a url'] as $url) {
        $this->postJson(route('native.open-external'), ['url' => $url])->assertStatus(422);
    }

    expect($shell->openExternalCalls)->toBe(['https://example.com/a?b=1&c=2', 'mailto:someone@example.com']);
});
