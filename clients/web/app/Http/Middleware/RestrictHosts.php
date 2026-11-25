<?php

namespace App\Http\Middleware;

use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * client-web has no login: whoever reaches it acts with this device's Director credential. A web page can rebind its
 * own name to 127.0.0.1 and so become same-origin with client-web (DNS rebinding), but its requests still carry that
 * name in Host. Only these hosts are answered; any other gets a 400 before anything runs:
 *   - the loopback names (localhost, 127.0.0.1, [::1]) the desktop app and the installed service use;
 *   - any IP address (a Tailscale address included): rebinding needs a name the attacker resolves, never an address;
 *   - APP_URL's host;
 *   - the names in CLIENT_WEB_TRUSTED_HOSTS (comma-separated), e.g. this machine's Tailscale MagicDNS name.
 */
class RestrictHosts
{
    /**
     * @param  Closure(Request): (Response)  $next
     */
    public function handle(Request $request, Closure $next): Response
    {
        if (! self::allows($request->getHost())) {
            return response('This host is not served here.', 400, ['Content-Type' => 'text/plain; charset=utf-8']);
        }

        return $next($request);
    }

    public static function allows(string $host): bool
    {
        $host = strtolower(rtrim($host, '.'));
        $bare = str_starts_with($host, '[') && str_ends_with($host, ']') ? substr($host, 1, -1) : $host;

        if ($bare === 'localhost' || filter_var($bare, FILTER_VALIDATE_IP) !== false) {
            return true;
        }

        return in_array($host, self::trustedNames(), true);
    }

    /**
     * @return list<string>
     */
    private static function trustedNames(): array
    {
        $names = explode(',', (string) config('client.trusted_hosts'));
        $names[] = (string) parse_url((string) config('app.url'), PHP_URL_HOST);

        return array_values(array_filter(array_map(fn (string $name): string => strtolower(rtrim(trim($name), '.')), $names)));
    }
}
