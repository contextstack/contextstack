<?php

namespace App\Http\Middleware;

use App\Client\Json;
use App\Native\AppLock;
use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * With the app lock on, nothing from the Director shows until the device unlock has succeeded in this launch. The
 * locked screen comes back to the screen that was asked for.
 */
class RequireUnlock
{
    public function __construct(private readonly AppLock $lock) {}

    /**
     * @param  Closure(Request): Response  $next
     */
    public function handle(Request $request, Closure $next): Response
    {
        if ($this->lock->locked()) {
            // The path goes back through the redirect; the query (a link's code) stays in memory, out of any URL.
            $this->lock->stoppedAt($request->getPathInfo(), Json::record($request->query()));

            return redirect('/locked');
        }

        return $next($request);
    }
}
