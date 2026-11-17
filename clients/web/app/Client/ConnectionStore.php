<?php

namespace App\Client;

use App\Client\Exceptions\NotConnected;
use JsonException;
use RuntimeException;

/**
 * This device's connection to its Director, kept in one settings file (0600 in a 0700 directory), never in the
 * database. A paired device keeps its token there; a client on the Director's own machine keeps only the URL and
 * reads the local credential file at each request, so a rotated local credential is picked up at once.
 */
class ConnectionStore
{
    public function __construct(private readonly string $settingsPath, private readonly string $defaultUrl, private readonly string $localCredentialPath) {}

    public static function fromConfig(): self
    {
        return new self(
            (string) config('client.settings_path'),
            (string) config('client.director_url'),
            (string) config('client.local_credential'),
        );
    }

    /**
     * The connection to use: the saved device token, else the local credential.
     *
     * @throws NotConnected
     */
    public function resolve(): Connection
    {
        $saved = $this->saved();

        if (($saved['mode'] ?? null) === Connection::Device && is_string($saved['token'] ?? null) && $saved['token'] !== '') {
            return new Connection(
                url: $this->normaliseUrl((string) ($saved['url'] ?? $this->defaultUrl)),
                token: $saved['token'],
                mode: Connection::Device,
                credentialId: is_string($saved['credential_id'] ?? null) ? $saved['credential_id'] : null,
                name: is_string($saved['name'] ?? null) ? $saved['name'] : null,
            );
        }

        $token = $this->localToken();

        if ($token === null) {
            throw new NotConnected('This device is not paired, and the local credential file cannot be read.');
        }

        return new Connection($this->normaliseUrl((string) ($saved['url'] ?? $this->defaultUrl)), $token, Connection::Local);
    }

    public function isConnected(): bool
    {
        try {
            $this->resolve();

            return true;
        } catch (NotConnected) {
            return false;
        }
    }

    public function hasLocalCredential(): bool
    {
        return $this->localToken() !== null;
    }

    /**
     * The URL this device would use, connected or not.
     */
    public function url(): string
    {
        return $this->normaliseUrl((string) ($this->saved()['url'] ?? $this->defaultUrl));
    }

    public function localCredentialPath(): string
    {
        return $this->localCredentialPath;
    }

    public function saveDevice(string $url, string $token, string $credentialId, string $name): void
    {
        $this->write(['mode' => Connection::Device, 'url' => $this->normaliseUrl($url), 'token' => $token,
            'credential_id' => $credentialId, 'name' => $name, 'paired_at' => now()->utc()->format('Y-m-d\TH:i:s\Z')]);
    }

    public function saveLocal(string $url): void
    {
        $this->write(['mode' => Connection::Local, 'url' => $this->normaliseUrl($url)]);
    }

    public function forget(): void
    {
        if (is_file($this->settingsPath)) {
            unlink($this->settingsPath);
        }
    }

    public function normaliseUrl(string $url): string
    {
        return rtrim(trim($url), '/');
    }

    private function localToken(): ?string
    {
        if ($this->localCredentialPath === '' || ! is_file($this->localCredentialPath) || ! is_readable($this->localCredentialPath)) {
            return null;
        }

        $token = trim((string) file_get_contents($this->localCredentialPath));

        return $token === '' ? null : $token;
    }

    /**
     * @return array<string, mixed>
     */
    private function saved(): array
    {
        if (! is_file($this->settingsPath)) {
            return [];
        }

        try {
            $decoded = json_decode((string) file_get_contents($this->settingsPath), true, 8, JSON_THROW_ON_ERROR);
        } catch (JsonException) {
            return [];
        }

        return is_array($decoded) ? $decoded : [];
    }

    /**
     * @param  array<string, string>  $settings
     */
    private function write(array $settings): void
    {
        $directory = dirname($this->settingsPath);

        if (! is_dir($directory) && ! mkdir($directory, 0700, true) && ! is_dir($directory)) {
            throw new RuntimeException("Cannot create {$directory}.");
        }

        $temporary = $this->settingsPath.'.'.bin2hex(random_bytes(4)).'.tmp';
        $previous = umask(0077);

        try {
            file_put_contents($temporary, json_encode($settings, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR)."\n");
            chmod($temporary, 0600);
            rename($temporary, $this->settingsPath);
        } finally {
            umask($previous);
        }
    }
}
