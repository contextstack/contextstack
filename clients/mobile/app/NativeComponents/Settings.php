<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Exceptions\TokenUnreadable;
use App\Client\IdentityStore;
use App\Client\Json;
use App\Native\AppLock;
use App\Native\DirectorScreen;
use App\NativeComponents\Members as MembersScreen;
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
 *
 * A member renames themselves here (`PATCH members/{id}` with `display_name` alone), without the device unlock: it
 * changes only their own name. An owner (`manage_org_settings`) also sets the Director's advertised URL, the origin its
 * invite and pairing links name (`GET`/`PUT advertised-url`); changing it asks for the device unlock, as member
 * changes do.
 */
class Settings extends DirectorScreen
{
    public string $url = '';

    public string $name = '';

    public string $credentialId = '';

    public bool $lockEnabled = false;

    public int $lockAfter = AppLock::DefaultAfter;

    /** This member's display name as it is being edited. */
    public string $myName = '';

    public string $nameProblem = '';

    /** Whether the Director answered `GET advertised-url` for this credential: the setting shows only then. */
    public bool $advertisedLoaded = false;

    /** The advertised URL the Director has, or null when none is set. */
    public ?string $advertisedUrl = null;

    public string $advertisedInput = '';

    public string $advertisedProblem = '';

    /**
     * The advertised-URL change waiting on the device unlock: the URL to set, or null to clear it.
     *
     * @var array{url: string|null}|array{}
     */
    public array $pendingAdvertised = [];

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
        $this->myName = (string) $this->identity()?->memberName;
        $this->readAdvertisedUrl();
    }

    /**
     * Whether this credential renames itself here: a member on a Director with teams.
     */
    public function mayRenameSelf(): bool
    {
        $identity = $this->identity();

        return $identity !== null && ! $identity->legacy() && $identity->memberId !== null;
    }

    public function renameMe(): void
    {
        $memberId = $this->identity()?->memberId;
        if (! $this->mayRenameSelf() || $memberId === null) {
            return;
        }
        $name = trim($this->myName);
        $this->nameProblem = MembersScreen::displayNameProblem($name);
        if ($this->nameProblem !== '' || $name === $this->identity()?->memberName) {
            return;
        }

        $key = 'rename:'.$memberId.':'.$name;
        try {
            $this->director()->put('members/'.rawurlencode($memberId), ['display_name' => $name], $this->keyFor($key), 'PATCH');
        } catch (DirectorRefused $refused) {
            $this->answered($key);
            $this->nameProblem = $refused->status === 401 ? '' : $this->refused($refused);

            return;
        } catch (DirectorUnavailable) {
            $this->nameProblem = 'The Director could not be reached, so the new name may not have arrived. Renaming again is safe.';

            return;
        }

        $this->answered($key);
        app(IdentityStore::class)->forget();
        $this->myName = $name;
        $this->nameProblem = '';
    }

    /**
     * Read the advertised URL for an owner. Anyone else, and a Director that predates the setting, sees nothing.
     */
    private function readAdvertisedUrl(): void
    {
        $this->advertisedLoaded = false;
        $identity = $this->identity();
        if ($identity === null || $identity->legacy() || ! $identity->can('manage_org_settings')) {
            return;
        }

        try {
            $this->advertisedUrl = Json::nullableString($this->director()->get('advertised-url')['url'] ?? null);
        } catch (DirectorRefused $refused) {
            if ($refused->status === 401) {
                $this->refused($refused);
            } elseif ($refused->status === 403) {
                app(IdentityStore::class)->forget();
            }

            return;
        } catch (DirectorUnavailable) {
            return;
        }

        $this->advertisedInput = (string) $this->advertisedUrl;
        $this->advertisedLoaded = true;
    }

    public function setAdvertisedUrl(): void
    {
        $url = trim($this->advertisedInput);
        if ($url === '') {
            $this->advertisedProblem = 'Enter the address other devices reach this Director at, such as https://director.example.ts.net, or clear it.';

            return;
        }
        $this->askAdvertised($url);
    }

    public function clearAdvertisedUrl(): void
    {
        $this->askAdvertised(null);
    }

    private function askAdvertised(?string $url): void
    {
        if (! $this->advertisedLoaded) {
            return;
        }
        $this->advertisedProblem = '';
        $this->pendingAdvertised = ['url' => $url];
        Biometrics::prompt()->id('advertised-url')->prompt();
    }

    private function saveAdvertisedUrl(?string $url): void
    {
        $key = 'advertised-url:'.($url ?? '');
        try {
            $answer = $this->director()->put('advertised-url', ['url' => $url], $this->keyFor($key));
        } catch (DirectorRefused $refused) {
            $this->answered($key);
            $this->advertisedProblem = $refused->status === 401 ? '' : $this->refused($refused);
            if ($refused->status === 403) {
                $this->readAdvertisedUrl();
            }

            return;
        } catch (DirectorUnavailable) {
            $this->advertisedProblem = 'The Director could not be reached, so the change may not have arrived. Saving again is safe.';

            return;
        }

        $this->answered($key);
        $this->advertisedUrl = Json::nullableString($answer['url'] ?? null);
        $this->advertisedInput = (string) $this->advertisedUrl;
        $this->advertisedProblem = '';
    }

    /**
     * Who this device acts for on the Director, from `GET me`: the member's name and role, or '' before teams.
     */
    public function memberLine(): string
    {
        $identity = $this->identity();
        if ($identity === null || $identity->memberId === null) {
            return '';
        }

        return trim(($identity->memberName !== '' ? $identity->memberName : $identity->memberId).' · '.$identity->roleWords(), ' ·');
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
        if ($id === 'advertised-url') {
            if ($this->pendingAdvertised === []) {
                return;
            }
            $url = $this->pendingAdvertised['url'];
            $this->pendingAdvertised = [];
            if (! $success) {
                $this->advertisedProblem = 'Not unlocked, so the advertised URL did not change.';

                return;
            }
            $this->saveAdvertisedUrl($url);

            return;
        }

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
        $this->forgetDirector();
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
