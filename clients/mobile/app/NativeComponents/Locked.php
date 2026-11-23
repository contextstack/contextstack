<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Native\AppLock;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Edge\NativeComponent;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * The app lock: asks for the device's own unlock (Face ID, Touch ID or the passcode) and then goes back to the screen
 * that was asked for. If this device can no longer unlock (the passcode was removed), forgetting the Director is the
 * way out: it gives nothing away, it only takes this device's access off the phone.
 */
class Locked extends NativeComponent
{
    public string $problem = '';

    public function mount(): void
    {
        if (! app(AppLock::class)->locked()) {
            $this->replace(app(AppLock::class)->returnTo());

            return;
        }

        $this->unlock();
    }

    public function unlock(): void
    {
        $this->problem = '';
        Biometrics::prompt()->id(AppLock::PromptId)->prompt();
    }

    #[On(Completed::class)]
    public function unlocked(bool $success, ?string $id = null): void
    {
        if ($id !== AppLock::PromptId) {
            return;
        }

        if (! $success) {
            $this->problem = 'Not unlocked. Try again.';

            return;
        }

        app(AppLock::class)->unlock();
        $this->replace(app(AppLock::class)->returnTo());
    }

    public function forget(): void
    {
        app(ConnectionStore::class)->forget();
        app(AppLock::class)->disable();
        $this->replace('/connect');
    }

    public function render(): View
    {
        return view('native.locked');
    }
}
