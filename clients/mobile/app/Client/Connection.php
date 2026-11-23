<?php

namespace App\Client;

/**
 * The Director this device talks to and the credential it uses: a paired device token, or the Director's local
 * credential read from its file on the same machine.
 */
final readonly class Connection
{
    public const string Device = 'device';

    public const string Local = 'local';

    public function __construct(
        public string $url,
        public string $token,
        public string $mode,
        public ?string $credentialId = null,
        public ?string $name = null,
    ) {}
}
