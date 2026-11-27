<?php

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\PendingRequest;
use Illuminate\Http\Client\Response;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Str;

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
     * POST /api/v1/pair: exchange a one-time code for this device's token. Needs no credential. A Director with teams
     * also names the member and role; `me` is read after saving the token either way.
     *
     * @return array{credential_id: string, name: string, token: string}
     */
    public static function pair(string $url, string $code, int $timeout = 15): array
    {
        return self::enroll($url, '/api/v1/pair', ['code' => trim($code)], 200, $timeout);
    }

    /**
     * POST /api/v1/invites/accept: join the Director as the invited member with a one-time invite code, and get this
     * device's token (the same shape as pairing). Needs no credential. A used, expired or revoked code is 403
     * `invite_refused`; too many tries are 429 with Retry-After.
     *
     * @return array{credential_id: string, name: string, token: string}
     */
    public static function acceptInvite(string $url, string $code, string $name, int $timeout = 15): array
    {
        return self::enroll($url, '/api/v1/invites/accept', ['code' => trim($code), 'name' => $name], 201, $timeout);
    }

    /**
     * An anonymous exchange of a one-time code for a device token. The code travels only in the body.
     *
     * @param  array<string, string>  $body
     * @return array{credential_id: string, name: string, token: string}
     */
    private static function enroll(string $url, string $path, array $body, int $status, int $timeout): array
    {
        try {
            $response = Http::acceptJson()->timeout($timeout)->post(rtrim($url, '/').$path, $body);
        } catch (ConnectionException $exception) {
            throw new DirectorUnavailable('The Director at '.$url.' could not be reached: '.$exception->getMessage(), 0, $exception);
        }

        $decoded = self::decode($response);

        if ($response->status() !== $status || ! is_string($decoded['token'] ?? null) || ! is_string($decoded['credential_id'] ?? null)) {
            throw self::refusal($response, $decoded);
        }

        return ['credential_id' => $decoded['credential_id'], 'name' => Json::string($decoded['name'] ?? null), 'token' => $decoded['token']];
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
     * A binary read (GET browser-action-screenshots/{sha}): the body as bytes when the Director answers 200 with the
     * expected media type and at most `$maxBytes`, read off the stream so a larger body is never held whole. Anything
     * else is a refusal: a JSON error body keeps its code, a wrong type or an oversized body is `unexpected_answer`.
     */
    public function bytes(string $path, string $mediaType, int $maxBytes): string
    {
        $response = $this->send(fn (PendingRequest $request) => $request->accept($mediaType)->withOptions(['stream' => true])->get($this->url($path)));
        if ($response->status() !== 200) {
            throw self::refusal($response, self::decode($response));
        }
        $type = strtolower(trim(explode(';', $response->header('Content-Type'))[0]));
        $declared = $response->header('Content-Length');
        if ($type !== $mediaType || ($declared !== '' && (int) $declared > $maxBytes)) {
            throw new DirectorRefused(200, 'unexpected_answer', 'The Director answered with something other than the expected '.$mediaType.' of at most '.$maxBytes.' bytes.', []);
        }

        $stream = $response->toPsrResponse()->getBody();
        $bytes = '';
        while (! $stream->eof() && strlen($bytes) <= $maxBytes) {
            $bytes .= $stream->read(min(65536, $maxBytes + 1 - strlen($bytes)));
        }
        $stream->close();
        if (strlen($bytes) > $maxBytes) {
            throw new DirectorRefused(200, 'unexpected_answer', 'The Director answered with more than '.$maxBytes.' bytes.', []);
        }

        return $bytes;
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

        return Json::record($decoded);
    }

    /**
     * @param  array<string, mixed>  $body
     */
    private static function refusal(Response $response, array $body): DirectorRefused
    {
        if ($response->status() === 429) {
            $body['retry_after'] = $response->header('Retry-After');
        }
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
