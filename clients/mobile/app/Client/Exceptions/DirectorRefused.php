<?php

namespace App\Client\Exceptions;

use RuntimeException;

/**
 * A Client API refusal, `{error, code, detail}` with any extra fields (CLIENT-API.md §9). Nothing was recorded.
 */
class DirectorRefused extends RuntimeException
{
    /**
     * @param  array<string, mixed>  $body
     */
    public function __construct(public readonly int $status, public readonly string $errorCode, public readonly string $detail, public readonly array $body = [])
    {
        parent::__construct("Director refused ({$status} {$errorCode}): {$detail}");
    }

    public function is(string $code): bool
    {
        return $this->errorCode === $code;
    }

    /**
     * Words a page can show.
     */
    public function explain(): string
    {
        return match ($this->errorCode) {
            'unauthenticated' => 'The Director does not accept this device\'s credential. It may have been revoked: pair again.',
            'owner_only' => 'Only the Director\'s owner can do that (403 owner_only), and this credential is not the owner\'s. Paired devices and the Director\'s own machine are owners; a lead\'s credential is limited to its own work.',
            'origin_refused', 'transport_refused' => 'The Director refused the connection: '.$this->detail,
            'idempotency_conflict' => 'An earlier request with this key was recorded and differs from this one.',
            'idempotency_key_required', 'idempotency_key_invalid' => 'The client sent a write without a usable Idempotency-Key: '.$this->detail,
            'objects_missing' => 'The Director is missing some bundle files.',
            'summary_running' => 'That summary is still being made; redo it once it ends.',
            'budgets_changed' => 'The gateway\'s budgets kept changing while the Director edited them; nothing was written. Try again.',
            'gateway_unavailable' => 'The Director could not reach its gateway: '.$this->detail,
            'gateway_refused' => 'The gateway refused: '.$this->detail,
            'gateway_owner_token' => 'The Director could not read the gateway\'s owner token on its machine: '.$this->detail,
            default => $this->detail !== '' ? $this->detail : "The Director refused the request ({$this->errorCode}).",
        };
    }
}
