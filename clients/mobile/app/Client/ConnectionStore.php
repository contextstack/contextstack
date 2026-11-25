<?php

namespace App\Client;

use App\Client\Exceptions\NotConnected;
use App\Client\Exceptions\TokenUnreadable;
use JsonException;
use Native\Mobile\Facades\SecureStorage;
use RuntimeException;

/**
 * This phone's connection to its Director, the only state the app keeps (the Director is the source of truth): the
 * URL, device name and credential id in one file (0600) in the app's container, and the device token in the iOS
 * Keychain (Android: EncryptedSharedPreferences) through the secure storage plugin. A token an earlier build saved in
 * the file moves to the Keychain the first time it is read. A phone never runs a Director, so there is no
 * local-credential mode here.
 */
class ConnectionStore
{
    public const string TokenKey = 'contextstack.director_token';

    public function __construct(private readonly string $settingsPath, private readonly string $defaultUrl) {}

    public static function fromConfig(): self
    {
        return new self(Json::string(config('client.settings_path')), Json::string(config('client.director_url')));
    }

    /**
     * @throws NotConnected
     */
    public function resolve(): Connection
    {
        $saved = $this->saved();

        if (Json::nullableString($saved['credential_id'] ?? null) === null) {
            throw new NotConnected('This device is not paired with a Director.');
        }

        $token = $this->token($saved);

        if ($token === null) {
            throw new TokenUnreadable('This device\'s token could not be read from the Keychain just now. Try again in a moment; if it keeps happening, forget this Director in Settings and pair again.');
        }

        return new Connection(
            url: $this->normaliseUrl(Json::string($saved['url'] ?? null, $this->defaultUrl)),
            token: $token,
            mode: Connection::Device,
            credentialId: is_string($saved['credential_id'] ?? null) ? $saved['credential_id'] : null,
            name: is_string($saved['name'] ?? null) ? $saved['name'] : null,
        );
    }

    /**
     * Whether this device is paired: a credential id is saved. A token the Keychain cannot give just now does not
     * make the device unpaired (TokenUnreadable).
     */
    public function isConnected(): bool
    {
        return Json::nullableString($this->saved()['credential_id'] ?? null) !== null;
    }

    /**
     * The URL this device would use, paired or not.
     */
    public function url(): string
    {
        return $this->normaliseUrl(Json::string($this->saved()['url'] ?? null, $this->defaultUrl));
    }

    public function saveDevice(string $url, string $token, string $credentialId, string $name): void
    {
        if (! SecureStorage::set(self::TokenKey, $token)) {
            throw new RuntimeException('The device token could not be saved in the Keychain.');
        }

        $this->write(['url' => $this->normaliseUrl($url), 'credential_id' => $credentialId,
            'name' => $name, 'paired_at' => now()->utc()->format('Y-m-d\TH:i:s\Z')]);
    }

    public function forget(): void
    {
        SecureStorage::delete(self::TokenKey);

        if (is_file($this->settingsPath)) {
            unlink($this->settingsPath);
        }
    }

    /**
     * The device token from the Keychain; a token still in the file (an earlier build) is moved there first.
     *
     * @param  array<string, mixed>  $saved
     */
    private function token(array $saved): ?string
    {
        $legacy = Json::nullableString($saved['token'] ?? null);

        if ($legacy !== null && SecureStorage::set(self::TokenKey, $legacy)) {
            unset($saved['token']);
            $this->write(array_map(fn (mixed $value): string => Json::string($value), $saved));

            return $legacy;
        }

        return Json::nullableString(SecureStorage::get(self::TokenKey)) ?? $legacy;
    }

    public function normaliseUrl(string $url): string
    {
        return rtrim(trim($url), '/');
    }

    /**
     * @return array<string, mixed>
     */
    public function saved(): array
    {
        if (! is_file($this->settingsPath)) {
            return [];
        }

        try {
            $decoded = json_decode((string) file_get_contents($this->settingsPath), true, 8, JSON_THROW_ON_ERROR);
        } catch (JsonException) {
            return [];
        }

        return Json::record($decoded);
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
