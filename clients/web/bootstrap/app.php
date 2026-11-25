<?php

use App\Http\Middleware\RestrictHosts;
use App\Http\Middleware\RestrictImages;
use Illuminate\Foundation\Application;
use Illuminate\Foundation\Configuration\Exceptions;
use Illuminate\Foundation\Configuration\Middleware;

$app = Application::configure(basePath: dirname(__DIR__))
    ->withRouting(
        web: __DIR__.'/../routes/web.php',
        health: '/up',
    )
    ->withMiddleware(function (Middleware $middleware): void {
        $middleware->prepend(RestrictHosts::class);
        $middleware->append(RestrictImages::class);
    })
    ->withExceptions(function (Exceptions $exceptions): void {
        //
    })->create();

// An installed release keeps its .env outside the code: DIRECTOR_ENV_FILE names an absolute env file,
// e.g. ~/.contextstack/config/director/director.env (0600). Unset, the app reads base_path('.env') as before.
$envFile = $_SERVER['DIRECTOR_ENV_FILE'] ?? $_ENV['DIRECTOR_ENV_FILE'] ?? getenv('DIRECTOR_ENV_FILE');
if (is_string($envFile) && $envFile !== '') {
    if (! str_starts_with($envFile, '/')) {
        throw new RuntimeException('DIRECTOR_ENV_FILE must be an absolute path.');
    }
    $app->useEnvironmentPath(dirname($envFile))->loadEnvironmentFrom(basename($envFile));
}

return $app;
