<?php

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\PendingRequest;
use Illuminate\Http\Client\Response;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Str;
use Psr\Http\Message\ResponseInterface;

/**
 * The Director's Client API (tests/director-spec/CLIENT-API.md), version 1, as one credential sees it. Every write
 * carries the caller's Idempotency-Key; a refusal becomes DirectorRefused and a lost answer DirectorUnavailable.
 */
class DirectorClient
{
    public const string Prefix = '/api/v1/client/';

    public function __construct(public readonly Connection $connection, private readonly int $timeout = 15) {}

    /**
     * A fresh key for one user action (CLIENT-API.md §7.2: a random UUID per action).
     */
    public static function newKey(): string
    {
        return (string) Str::uuid();
    }

    /**
     * POST /api/v1/pair: exchange a one-time code for this device's token. Needs no credential.
     *
     * @return array{credential_id: string, name: string, token: string}
     */
    public static function pair(string $url, string $code, int $timeout = 15): array
    {
        try {
            $response = Http::acceptJson()->timeout($timeout)->post(rtrim($url, '/').'/api/v1/pair', ['code' => trim($code)]);
        } catch (ConnectionException $exception) {
            throw new DirectorUnavailable('The Director at '.$url.' could not be reached: '.$exception->getMessage(), 0, $exception);
        }

        $body = self::decode($response);

        if ($response->status() !== 200 || ! is_string($body['token'] ?? null) || ! is_string($body['credential_id'] ?? null)) {
            throw self::refusal($response, $body);
        }

        return ['credential_id' => $body['credential_id'], 'name' => (string) ($body['name'] ?? ''), 'token' => $body['token']];
    }

    /**
     * @param  array<string, scalar|null>  $query
     * @return array<string, mixed>
     */
    public function get(string $path, array $query = []): array
    {
        $response = $this->send(fn (PendingRequest $request) => $request->get($this->url($path), array_filter($query, fn ($value) => $value !== null && $value !== '')));
        $body = self::decode($response);

        if ($response->status() !== 200) {
            throw self::refusal($response, $body);
        }

        return $body;
    }

    /**
     * A read that answers 404 for "none yet" (GET runs/{id}/decision).
     *
     * @return array<string, mixed>|null
     */
    public function find(string $path): ?array
    {
        try {
            return $this->get($path);
        } catch (DirectorRefused $refused) {
            if ($refused->status === 404) {
                return null;
            }

            throw $refused;
        }
    }

    /**
     * A write under the write rules (CLIENT-API.md §7): 201/202 recorded, 200 with Idempotent-Replayed a replay.
     * 409, 422 and 428 are refusals: nothing was recorded, and the key is not used up.
     *
     * @param  array<string, mixed>  $body
     */
    public function write(string $path, array $body, string $key): WriteAnswer
    {
        $response = $this->send(fn (PendingRequest $request) => $request->withHeaders(['Idempotency-Key' => $key])
            ->withBody(json_encode((object) $body, JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR), 'application/json')
            ->post($this->url($path)));
        $decoded = self::decode($response);

        if (! in_array($response->status(), [200, 201, 202], true)) {
            throw self::refusal($response, $decoded);
        }

        return new WriteAnswer($response->status(), $decoded, $response->header('Idempotent-Replayed') === 'true');
    }

    /**
     * A PUT or PATCH under the write rules: an Idempotency-Key, 200 the answer, anything else a refusal (the
     * chief-of-staff slot; a conversation's title).
     *
     * @param  array<string, mixed>  $body
     * @return array<string, mixed>
     */
    public function put(string $path, array $body, string $key, string $method = 'PUT'): array
    {
        $response = $this->send(fn (PendingRequest $request) => $request->withHeaders(['Idempotency-Key' => $key])
            ->withBody(json_encode((object) $body, JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR), 'application/json')
            ->send($method, $this->url($path)));
        $decoded = self::decode($response);

        if ($response->status() !== 200) {
            throw self::refusal($response, $decoded);
        }

        return $decoded;
    }

    /**
     * A DELETE under the write rules: an Idempotency-Key, 200 the answer, anything else a refusal (a conversation
     * delete: 409 `busy` while a message runs, 403 for a lead's credential, 404 once it is gone).
     *
     * @return array<string, mixed>
     */
    public function remove(string $path, string $key): array
    {
        $response = $this->send(fn (PendingRequest $request) => $request->withHeaders(['Idempotency-Key' => $key])->delete($this->url($path)));
        $decoded = self::decode($response);

        if ($response->status() !== 200) {
            throw self::refusal($response, $decoded);
        }

        return $decoded;
    }

    /**
     * A write that replaces what it names, so it needs no key (CLIENT-API.md §7, §12: the budget writes). PUT with
     * a JSON body, or DELETE; 200 is the answer, anything else a refusal.
     *
     * @param  array<string, mixed>|null  $body  null for DELETE
     * @return array<string, mixed>
     */
    public function replace(string $path, ?array $body): array
    {
        $response = $this->send(fn (PendingRequest $request) => $body === null
            ? $request->delete($this->url($path))
            : $request->withBody(json_encode((object) $body, JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR), 'application/json')->put($this->url($path)));
        $decoded = self::decode($response);

        if ($response->status() !== 200) {
            throw self::refusal($response, $decoded);
        }

        return $decoded;
    }

    /**
     * PUT objects/{sha256}: one bundle file's bytes. Needs no key: the name is the content.
     */
    public function putObject(string $bytes): string
    {
        $sha256 = hash('sha256', $bytes);
        $response = $this->send(fn (PendingRequest $request) => $request->withBody($bytes, 'application/octet-stream')
            ->put($this->url('objects/'.$sha256)));

        if ($response->status() !== 200) {
            throw self::refusal($response, self::decode($response));
        }

        return $sha256;
    }

    /**
     * POST intents with the bundle's files (CLIENT-API.md §4.1).
     *
     * @param  array<string, mixed>  $intent  the intent body without files
     */
    public function submitIntent(array $intent, Bundle $bundle, string $key): WriteAnswer
    {
        return $this->writeWithBundle('intents', $intent, $bundle, $key);
    }

    /**
     * A write that carries a bundle (POST intents, POST conversations): upload every object first, then send the
     * body with the bundle's file list. If the Director still names missing objects (422 objects_missing), upload
     * those and send the same request again with the same key.
     *
     * @param  array<string, mixed>  $body  the body without files
     */
    public function writeWithBundle(string $path, array $body, Bundle $bundle, string $key): WriteAnswer
    {
        foreach ($bundle->contents() as $bytes) {
            $this->putObject($bytes);
        }

        $body = [...$body, 'files' => $bundle->files()];

        try {
            return $this->write($path, $body, $key);
        } catch (DirectorRefused $refused) {
            if (! $refused->is('objects_missing')) {
                throw $refused;
            }

            $missing = array_filter((array) ($refused->body['missing'] ?? []), 'is_string');
            foreach ($bundle->contents() as $sha256 => $bytes) {
                if (in_array($sha256, $missing, true)) {
                    $this->putObject($bytes);
                }
            }

            return $this->write($path, $body, $key);
        }
    }

    /**
     * GET jobs/{id}/files/{path}: one file of a job's bundle, its bytes.
     */
    public function jobFile(string $jobId, string $path): string
    {
        $response = $this->file('jobs', $jobId, $path);

        if ($response->status() !== 200) {
            throw self::refusal($response, self::decode($response));
        }

        return $response->body();
    }

    /**
     * GET runs/{id}/files/{path} or jobs/{id}/files/{path}: the raw answer, for relaying evidence.
     */
    public function file(string $kind, string $id, string $path): Response
    {
        return $this->send(fn (PendingRequest $request) => $request->get($this->url($kind.'/'.rawurlencode($id).'/files/'.self::encodePath($path))));
    }

    /**
     * GET events, held open, each chunk handed to $forward as it arrives (CLIENT-API.md §8). Ends when the Director
     * ends the stream, after $seconds, or when $forward returns false (the receiver is gone). With a conversation id,
     * only that conversation's events (`GET events?conversation=ID`).
     *
     * @param  callable(string): bool  $forward
     * @return array{status: int, body: string} the Director's status; for a refusal, its body
     */
    public function relayEvents(callable $forward, int $seconds, ?string $conversation = null): array
    {
        $sink = new PassThroughStream($forward);

        try {
            $response = Http::withToken($this->connection->token)
                ->withHeaders(['Accept' => 'text/event-stream'])
                ->withOptions([
                    'sink' => $sink,
                    'on_headers' => function (ResponseInterface $response) use ($sink): void {
                        $sink->status = $response->getStatusCode();
                    },
                    // curl calls this about once a second even when no bytes come; a non-zero answer ends the
                    // transfer, so a browser that left frees its worker within a second or two.
                    'curl' => [
                        CURLOPT_NOPROGRESS => false,
                        CURLOPT_XFERINFOFUNCTION => fn (): int => $sink->stillWanted() ? 0 : 1,
                    ],
                ])
                ->connectTimeout($this->timeout)
                ->timeout($seconds)
                ->get($this->url('events'), $conversation !== null ? ['conversation' => $conversation] : []);
        } catch (ConnectionException $exception) {
            if ($sink->status === null) {
                throw new DirectorUnavailable('The Director could not be reached: '.$exception->getMessage(), 0, $exception);
            }

            // The stream ran its time, dropped, or the receiver left: an ordinary end.
            return ['status' => $sink->status, 'body' => $sink->held()];
        }

        // A handler that keeps the body instead of writing it to the sink (a fake) leaves it on the response.
        $body = $sink->held() !== '' || $sink->forwardedAny() ? $sink->held() : $response->body();
        if ($response->status() === 200 && $body !== '') {
            $forward($body);

            return ['status' => 200, 'body' => ''];
        }

        return ['status' => $response->status(), 'body' => $body];
    }

    public static function encodePath(string $path): string
    {
        return implode('/', array_map('rawurlencode', explode('/', $path)));
    }

    private function url(string $path): string
    {
        return $this->connection->url.self::Prefix.ltrim($path, '/');
    }

    /**
     * @param  callable(PendingRequest): Response  $call
     */
    private function send(callable $call): Response
    {
        try {
            return $call(Http::withToken($this->connection->token)->acceptJson()->timeout($this->timeout));
        } catch (ConnectionException $exception) {
            throw new DirectorUnavailable('The Director could not be reached: '.$exception->getMessage(), 0, $exception);
        }
    }

    /**
     * @return array<string, mixed>
     */
    private static function decode(Response $response): array
    {
        $decoded = json_decode($response->body(), true);

        return is_array($decoded) ? $decoded : [];
    }

    /**
     * @param  array<string, mixed>  $body
     */
    private static function refusal(Response $response, array $body): DirectorRefused
    {
        if ($response->status() >= 500 && ! isset($body['code'])) {
            return new DirectorRefused($response->status(), 'director_error', 'The Director answered '.$response->status().'.', $body);
        }

        return new DirectorRefused(
            $response->status(),
            is_string($body['code'] ?? null) ? $body['code'] : 'unexpected_answer',
            is_string($body['detail'] ?? null) ? $body['detail'] : 'The Director answered '.$response->status().'.',
            $body,
        );
    }
}
