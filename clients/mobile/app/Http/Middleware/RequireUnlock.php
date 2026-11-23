<?php

namespace App\Http\Middleware;

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
            $this->lock->stoppedAt($request->getPathInfo());

            return redirect('/locked');
        }

        return $next($request);
    }
}
