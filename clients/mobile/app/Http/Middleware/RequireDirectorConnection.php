<?php

namespace App\Http\Middleware;

use App\Client\ConnectionStore;
use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * Screens that read the Director need a paired device. An unpaired phone goes to the connect screen; NativePHP's
 * screen guard turns the redirect into a replace onto that native screen.
 */
class RequireDirectorConnection
{
    public function __construct(private readonly ConnectionStore $connections) {}

    /**
     * @param  Closure(Request): Response  $next
     */
    public function handle(Request $request, Closure $next): Response
    {
        if (! $this->connections->isConnected()) {
            return redirect('/connect');
        }

        return $next($request);
    }
}
