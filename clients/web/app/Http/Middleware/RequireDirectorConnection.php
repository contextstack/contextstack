<?php

namespace App\Http\Middleware;

use App\Client\ConnectionStore;
use App\Client\DesktopConnection;
use App\Client\NativeDesktop;
use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * client-web pages need a credential for the Director: a paired device token or the local credential. Without one,
 * the person is sent to the connection page.
 */
class RequireDirectorConnection
{
    public function __construct(
        private readonly ConnectionStore $connections,
        private readonly DesktopConnection $desktop,
    ) {}

    /**
     * @param  Closure(Request): (Response)  $next
     */
    public function handle(Request $request, Closure $next): Response
    {
        if (NativeDesktop::isRunning()) {
            if ($this->desktop->isReady()) {
                return $next($request);
            }

            return $request->expectsJson()
                ? response()->json(['error' => 'Not connected', 'code' => 'not_connected', 'detail' => 'ContextStack is not connected on this Mac.'], 409)
                : redirect()->route('desktop.gate');
        }

        if (! $this->connections->isConnected()) {
            return $request->expectsJson()
                ? response()->json(['error' => 'Not connected', 'code' => 'not_connected', 'detail' => 'Pair this device with a Director first.'], 409)
                : redirect()->route('connect');
        }

        return $next($request);
    }
}
