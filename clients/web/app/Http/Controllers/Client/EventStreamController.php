<?php

namespace App\Http\Controllers\Client;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Identity;
use App\Http\Controllers\Controller;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\StreamedResponse;

/**
 * Relays the Director's event stream (CLIENT-API.md §8) to this device's browser. The browser never holds the
 * Director credential, and tokens never travel in URLs: client-web opens `GET events` with its credential and
 * passes each chunk through unchanged as it arrives. The browser follows the reconnect order on whatever ends the
 * stream.
 *
 * The relay holds a PHP worker (`php -S` has PHP_CLI_SERVER_WORKERS, eight), so it never lifts PHP's time limit: it ends itself after
 * `client.stream_seconds` or 5 s before `max_execution_time`, whichever is sooner. A relay that ran its time ends
 * with `retry: 250` and one `event: relay_end` frame (client-web's own, not the Director's): a planned end, which the
 * browser treats as a quiet reconnect. Without it, PHP killed the relay at 30 s with a fatal error.
 *
 * `?conversation=ID` relays only that conversation's events (CLIENT-API.md §8, `GET events?conversation=`).
 *
 * A refusal or an unreachable Director becomes one `event: director_error` frame, then the stream ends.
 *
 * Who this device is (Identity) is asked again once the stream ends, however it ends, and as soon as the Director says
 * `authorization_changed` or `revoked`: the member's role or the credential changed (CLIENT-API.md §8). The browser
 * reloads the page on those two, so every control is drawn again from the new capabilities.
 */
class EventStreamController extends Controller
{
    public function __invoke(Request $request, DirectorClient $director, Identity $identity): StreamedResponse
    {
        $seconds = self::seconds((int) config('client.stream_seconds'), (int) ini_get('max_execution_time'));
        $conversation = $request->query('conversation');
        $conversation = is_string($conversation) && preg_match('/^[A-Za-z0-9_.:-]{1,128}$/', $conversation) === 1 ? $conversation : null;

        return response()->stream(function () use ($director, $identity, $seconds, $conversation): void {
            $started = now();
            try {
                $answer = $director->relayEvents(function (string $chunk) use ($identity): bool {
                    if (self::changesIdentity($chunk)) {
                        $identity->forget();
                    }
                    echo $chunk;
                    $this->flush();

                    return connection_aborted() !== 1;
                }, $seconds, $conversation);
            } catch (DirectorUnavailable $unavailable) {
                $this->emitError(0, 'director_unavailable', $unavailable->getMessage());

                return;
            } finally {
                $identity->forget();
            }

            if ($answer['status'] === 200 && $started->diffInSeconds(now()) >= $seconds - 1 && connection_aborted() !== 1) {
                echo "retry: 250\nevent: relay_end\ndata: {\"reason\":\"time\"}\n\n";
                $this->flush();

                return;
            }

            if ($answer['status'] !== 200) {
                $body = json_decode($answer['body'], true);
                $this->emitError($answer['status'], is_array($body) && is_string($body['code'] ?? null) ? $body['code'] : 'refused',
                    is_array($body) && is_string($body['detail'] ?? null) ? $body['detail'] : 'The Director refused the stream.');
            }
        }, 200, [
            'Content-Type' => 'text/event-stream',
            'Cache-Control' => 'no-cache, no-store',
            'X-Accel-Buffering' => 'no',
        ]);
    }

    /**
     * How long one relay may run: the configured time, but always 5 s inside PHP's limit (0 = none); at least 1 s.
     */
    public static function seconds(int $configured, int $maxExecution): int
    {
        $seconds = max(5, $configured);
        if ($maxExecution > 0) {
            $seconds = min($seconds, $maxExecution > 10 ? $maxExecution - 5 : intdiv($maxExecution, 2));
        }

        return max(1, $seconds);
    }

    /**
     * Whether a relayed chunk ends this credential's standing: an `authorization_changed` change or `event: revoked`.
     * A frame split across chunks is still caught when the stream then ends.
     */
    public static function changesIdentity(string $chunk): bool
    {
        return str_contains($chunk, '"authorization_changed"') || preg_match('/^event: ?revoked$/m', $chunk) === 1;
    }

    private function emitError(int $status, string $code, string $detail): void
    {
        echo "event: director_error\ndata: ".json_encode(['status' => $status, 'code' => $code, 'detail' => $detail], JSON_UNESCAPED_SLASHES)."\n\n";
        $this->flush();
    }

    private function flush(): void
    {
        if (ob_get_level() > 0) {
            ob_flush();
        }
        flush();
    }
}
