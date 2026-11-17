<?php

namespace App\Client;

/**
 * The Director's answer to a recorded write (CLIENT-API.md §7): the record, and whether it was a replay of an
 * earlier request with the same Idempotency-Key and content.
 */
final readonly class WriteAnswer
{
    /**
     * @param  array<string, mixed>  $body
     */
    public function __construct(public int $status, public array $body, public bool $replayed) {}
}
