<?php

use Illuminate\Support\Facades\File;
use Symfony\Component\Process\Process;

/*
 * An installed release reads its env from DIRECTOR_ENV_FILE, outside the code, and keeps every runtime path
 * (storage, the Director's local credential) under the install root. The template it is seeded from holds
 * no secrets, and it names no database: client-web keeps none (Director.Plan step 8).
 */

/** @return array<string, string> */
function installTemplate(): array
{
    $values = [];
    foreach (file(base_path('.env.install.example'), FILE_IGNORE_NEW_LINES) ?: [] as $line) {
        if (preg_match('/^([A-Z][A-Z0-9_]*)=(.*)$/', $line, $match) === 1) {
            $values[$match[1]] = $match[2];
        }
    }

    return $values;
}

/** @param array<string, string|false> $extra */
function bootWithEnvFile(string $envFile, array $extra = []): Process
{
    // The child must see only the env file: drop what phpunit.xml and the template would otherwise inherit.
    $inherited = ['APP_ENV', 'APP_KEY', 'APP_DEBUG', 'APP_URL', 'DB_CONNECTION', 'DB_DATABASE', 'DB_URL',
        'CACHE_STORE', 'QUEUE_CONNECTION', 'SESSION_DRIVER', 'MAIL_MAILER', 'BROADCAST_CONNECTION',
        'BCRYPT_ROUNDS', 'APP_MAINTENANCE_DRIVER', 'LARAVEL_STORAGE_PATH', 'DIRECTOR_URL', 'DIRECTOR_LOCAL_CREDENTIAL',
        'CLIENT_WEB_SETTINGS', 'CLIENT_WEB_STREAM_SECONDS', 'CLIENT_WEB_DIRECTOR_TIMEOUT'];
    $environment = array_fill_keys([...$inherited, ...array_keys(installTemplate())], false);
    $code = <<<'PHP'
        require 'vendor/autoload.php';
        $app = require 'bootstrap/app.php';
        $app->make(Illuminate\Contracts\Console\Kernel::class)->bootstrap();
        echo json_encode([
            'env' => $app->environment(),
            'debug' => config('app.debug'),
            'storage' => storage_path(),
            'views' => config('view.compiled'),
            'session' => config('session.driver'),
            'cache' => config('cache.default'),
            'queue' => config('queue.default'),
            'director_url' => config('client.director_url'),
            'local_credential' => config('client.local_credential'),
            'settings' => config('client.settings_path'),
            'key_set' => is_string(config('app.key')) && config('app.key') !== '',
        ]);
        PHP;

    return new Process([PHP_BINARY, '-r', $code], base_path(), [...$environment, 'DIRECTOR_ENV_FILE' => $envFile, ...$extra], timeout: 30);
}

it('ships an install env template with no secret values', function () {
    $template = installTemplate();

    expect($template)->toHaveKeys(['APP_KEY', 'APP_ENV', 'APP_DEBUG', 'LARAVEL_STORAGE_PATH', 'DIRECTOR_URL', 'DIRECTOR_LOCAL_CREDENTIAL'])
        ->and(array_filter(array_keys($template), fn (string $key): bool => str_starts_with($key, 'DB_')))->toBe([])
        ->and($template['APP_KEY'])->toBe('')
        ->and($template['APP_ENV'])->toBe('production')
        ->and($template['APP_DEBUG'])->toBe('false');
    foreach ($template as $key => $value) {
        if (preg_match('/(KEY|SECRET|PASSWORD|TOKEN)$/', $key) === 1) {
            expect($value)->toBe('', "{$key} must be empty in the template");
        }
    }
});

it('keeps every runtime path of the template under the install root', function () {
    $template = installTemplate();

    foreach (['LARAVEL_STORAGE_PATH', 'DIRECTOR_LOCAL_CREDENTIAL'] as $key) {
        expect($template[$key])->toStartWith('__ROOT__/');
    }
    expect($template['APP_URL'])->toBe('__URL__')
        ->and($template['DIRECTOR_URL'])->toBe('__DIRECTOR_URL__')
        ->and($template['LARAVEL_STORAGE_PATH'])->toBe('__ROOT__/state/client-web');
});

it('boots from DIRECTOR_ENV_FILE with storage outside the code and no database', function () {
    $root = sys_get_temp_dir().'/director-install-env-'.bin2hex(random_bytes(4));
    foreach (['config/client-web', 'state/client-web/framework/views', 'state/client-web/framework/cache/data',
        'state/client-web/framework/sessions', 'state/client-web/logs'] as $dir) {
        File::ensureDirectoryExists("{$root}/{$dir}");
    }
    $env = str_replace(['__ROOT__', '__URL__', '__DIRECTOR_URL__'], [$root, 'http://127.0.0.1:18790', 'http://127.0.0.1:8780'],
        (string) file_get_contents(base_path('.env.install.example')));
    $env = preg_replace('/^APP_KEY=$/m', 'APP_KEY=base64:'.base64_encode(random_bytes(32)), $env);
    file_put_contents("{$root}/config/client-web/client-web.env", $env);

    try {
        $process = bootWithEnvFile("{$root}/config/client-web/client-web.env");
        $process->mustRun();
        $seen = json_decode($process->getOutput(), true);

        expect($seen)->toMatchArray([
            'env' => 'production',
            'debug' => false,
            'storage' => "{$root}/state/client-web",
            'views' => realpath("{$root}/state/client-web/framework/views"),
            'session' => 'file',
            'cache' => 'file',
            'queue' => 'sync',
            'director_url' => 'http://127.0.0.1:8780',
            'local_credential' => "{$root}/state/director/local-credential",
            'settings' => "{$root}/state/client-web/app/private/client-web/connection.json",
            'key_set' => true,
        ]);
    } finally {
        File::deleteDirectory($root);
    }
});

it('refuses a relative DIRECTOR_ENV_FILE', function () {
    $process = bootWithEnvFile('config/client-web.env');
    $process->run();

    expect($process->isSuccessful())->toBeFalse()
        ->and($process->getErrorOutput().$process->getOutput())->toContain('DIRECTOR_ENV_FILE must be an absolute path.');
});
