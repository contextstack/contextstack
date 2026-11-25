<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Client\Exceptions\NotConnected;
use App\Client\Exceptions\TokenUnreadable;
use App\Native\AppLock;
use App\Native\DirectorScreen;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * This device's connection: which Director, under which paired credential. Forgetting it removes the token from the
 * phone; revoke the credential on the Director's machine as well if the phone is lost.
 *
 * The app lock is chosen here. Turning it on or off first asks for the device unlock, so the owner proves this device
 * can unlock before it can lock them out, and someone holding an unlocked app cannot quietly turn it off.
 */
class Settings extends DirectorScreen
{
    public string $url = '';

    public string $name = '';

    public string $credentialId = '';

    public bool $lockEnabled = false;

    public int $lockAfter = AppLock::DefaultAfter;

    public function mount(): void
    {
        try {
            $connection = app(ConnectionStore::class)->resolve();
        } catch (NotConnected) {
            $this->replace('/connect');

            return;
        } catch (TokenUnreadable $unreadable) {
            $this->problem = $unreadable->getMessage();

            return;
        }

        $this->url = $connection->url;
        $this->name = (string) $connection->name;
        $this->credentialId = (string) $connection->credentialId;
        $this->readLock();
    }

    public function turnOnLock(): void
    {
        $this->problem = '';
        Biometrics::prompt()->id('lock-on')->prompt();
    }

    public function turnOffLock(): void
    {
        $this->problem = '';
        Biometrics::prompt()->id('lock-off')->prompt();
    }

    public function lockAfterSeconds(int $seconds): void
    {
        app(AppLock::class)->setAfterSeconds($seconds);
        $this->readLock();
    }

    #[On(Completed::class)]
    public function unlocked(bool $success, ?string $id = null): void
    {
        if ($id !== 'lock-on' && $id !== 'lock-off') {
            return;
        }

        if (! $success) {
            $this->problem = 'Not unlocked, so the app lock did not change.';

            return;
        }

        $id === 'lock-on' ? app(AppLock::class)->enable() : app(AppLock::class)->disable();
        $this->problem = '';
        $this->readLock();
    }

    public function forget(): void
    {
        app(ConnectionStore::class)->forget();
        app(AppLock::class)->disable();
        $this->replace('/connect');
    }

    public function render(): View
    {
        return view('native.settings', ['lockChoices' => AppLock::Choices]);
    }

    private function readLock(): void
    {
        $lock = app(AppLock::class);
        $this->lockEnabled = $lock->enabled();
        $this->lockAfter = $lock->afterSeconds();
    }
}
