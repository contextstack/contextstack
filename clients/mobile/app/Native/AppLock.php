<?php

namespace App\Native;

use App\Client\Json;
use Illuminate\Support\Facades\File;

/**
 * The optional app lock: when it is on, the app asks for the device's own unlock (iOS device-owner authentication:
 * Face ID, Touch ID or the passcode, whichever the owner set up) before it shows anything from the Director, at
 * every launch and after it has been away for `afterSeconds`.
 *
 * The choice (on or off, and the time away) is a preference of this device, kept in one file beside the connection.
 * Whether the app is unlocked lives only in this object: the mobile runtime boots once per launch and keeps container
 * singletons between screens, so a cold launch, or the OS ending PHP in the background, starts locked again.
 */
class AppLock
{
    /** The times away the owner can choose, in seconds, with the words Settings shows for them. */
    public const Choices = [0 => 'Immediately', 60 => 'After 1 minute', 300 => 'After 5 minutes', 900 => 'After 15 minutes'];

    public const DefaultAfter = 60;

    public const PromptId = 'app-lock';

    /**
     * A return sooner than this never locks, even on Immediately: the Face ID or passcode sheet, and the scanner,
     * briefly take the app out of the foreground, and locking on that return would undo the unlock just given.
     */
    public const Glance = 2;

    private bool $unlocked = false;

    /** The screen that was asked for when the lock stepped in, to go back to once unlocked. */
    private string $returnTo = '/';

    public function __construct(private readonly string $path) {}

    public static function fromConfig(): self
    {
        $path = config('client.lock_path');

        return new self(is_string($path) ? $path : storage_path('app/private/client-mobile/lock.json'));
    }

    public function enabled(): bool
    {
        return ($this->read()['enabled'] ?? false) === true;
    }

    public function afterSeconds(): int
    {
        $after = $this->read()['after_seconds'] ?? null;

        return is_int($after) && array_key_exists($after, self::Choices) ? $after : self::DefaultAfter;
    }

    public function locked(): bool
    {
        return $this->enabled() && ! $this->unlocked;
    }

    /**
     * Turning the lock on counts as an unlock: Settings only calls this after the device unlock has just succeeded,
     * which also proves this device can unlock at all before it can lock the owner out.
     */
    public function enable(): void
    {
        $this->write(['enabled' => true, 'after_seconds' => $this->afterSeconds()]);
        $this->unlocked = true;
    }

    public function disable(): void
    {
        $this->write(['enabled' => false, 'after_seconds' => $this->afterSeconds()]);
    }

    public function setAfterSeconds(int $seconds): void
    {
        if (! array_key_exists($seconds, self::Choices)) {
            return;
        }

        $this->write(['enabled' => $this->enabled(), 'after_seconds' => $seconds]);
    }

    public function unlock(): void
    {
        $this->unlocked = true;
    }

    /**
     * NativePHP's screen guard keeps only the path of a redirect, so the screen to return to is held here.
     */
    public function stoppedAt(string $path): void
    {
        $this->returnTo = str_starts_with($path, '/') && ! str_starts_with($path, '//') && $path !== '/locked' ? $path : '/';
    }

    public function returnTo(): string
    {
        return $this->returnTo;
    }

    /**
     * The app came back after `$secondsAway` (measured natively, counting while the device sleeps).
     */
    public function returned(int $secondsAway): void
    {
        if ($secondsAway >= max($this->afterSeconds(), self::Glance)) {
            $this->unlocked = false;
            $this->returnTo = '/';
        }
    }

    /**
     * @return array<string, mixed>
     */
    private function read(): array
    {
        if (! File::exists($this->path)) {
            return [];
        }

        return Json::record(json_decode((string) File::get($this->path), true));
    }

    /**
     * @param  array{enabled: bool, after_seconds: int}  $settings
     */
    private function write(array $settings): void
    {
        File::ensureDirectoryExists(dirname($this->path), 0700);
        File::put($this->path, (string) json_encode($settings, JSON_PRETTY_PRINT));
    }
}
