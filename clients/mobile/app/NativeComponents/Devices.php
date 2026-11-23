<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\DirectorScreen;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * Devices paired with the Director: list them, pair another one, revoke one. Pairing and revoking first ask for this
 * device's own unlock (iOS device-owner authentication: Face ID, Touch ID or the passcode, whichever the owner set
 * up), so a lost but unlocked-once phone cannot hand out access by itself. This device cannot revoke itself here;
 * Settings forgets it instead.
 */
class Devices extends DirectorScreen
{
    /** @var list<array<string, mixed>> */
    public array $devices = [];

    public string $thisDevice = '';

    public string $name = '';

    /** @var array{code: string, name: string, expires_at: string}|array{} */
    public array $issued = [];

    public bool $allowed = true;

    public bool $loaded = false;

    public function mount(): void
    {
        $this->thisDevice = (string) app(ConnectionStore::class)->resolve()->credentialId;
        $this->refresh();
    }

    public function refresh(): void
    {
        try {
            $credentials = Json::records($this->director()->get('credentials')['credentials'] ?? null);
        } catch (DirectorRefused $refused) {
            $this->allowed = ! $refused->is('owner_only');
            $this->problem = $this->allowed ? $refused->explain() : '';
            $this->loaded = true;

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached. Check that Tailscale is on, then try again.';

            return;
        }

        $this->allowed = true;
        $this->problem = '';
        $this->devices = array_values(array_filter($credentials, fn (array $credential): bool => ($credential['kind'] ?? null) === 'device'));
        usort($this->devices, fn (array $a, array $b): int => strcmp(Json::string($b['created_at'] ?? null), Json::string($a['created_at'] ?? null)));
        $this->loaded = true;
    }

    public function pair(?string $submitted = null): void
    {
        $name = trim($submitted ?? $this->name);
        if (preg_match('/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/', $name) !== 1) {
            $this->problem = 'Use letters, numbers, dots, dashes or underscores, starting with a letter or number (for example phone).';

            return;
        }
        $this->name = $name;
        Biometrics::prompt()->id('pair')->prompt();
    }

    public function revoke(string $id): void
    {
        if ($id === $this->thisDevice || ! in_array($id, array_map(fn (array $device): string => Json::string($device['id'] ?? null), $this->devices), true)) {
            return;
        }
        Biometrics::prompt()->id('revoke:'.$id)->prompt();
    }

    /**
     * The device unlock answered. Only a success for the action that asked goes on to the Director.
     */
    #[On(Completed::class)]
    public function unlocked(bool $success, ?string $id = null): void
    {
        if (! $success) {
            $this->problem = 'Not unlocked, so nothing changed.';

            return;
        }

        if ($id === 'pair') {
            $this->issue();
        } elseif (is_string($id) && str_starts_with($id, 'revoke:')) {
            $this->remove(substr($id, 7));
        }
    }

    public function done(): void
    {
        $this->issued = [];
        $this->refresh();
    }

    public function render(): View
    {
        return view('native.devices', ['address' => app(ConnectionStore::class)->url()]);
    }

    private function issue(): void
    {
        try {
            $answer = $this->director()->write('pairings', ['name' => $this->name], DirectorClient::newKey());
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->is('owner_only') ? 'The Director does not let this credential pair devices.' : $refused->explain();

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so no code was issued.';

            return;
        }

        $this->problem = '';
        $this->issued = [
            'code' => Json::string($answer->body['code'] ?? null),
            'name' => Json::string($answer->body['name'] ?? null, $this->name),
            'expires_at' => Json::string($answer->body['expires_at'] ?? null),
        ];
        $this->name = '';
    }

    private function remove(string $id): void
    {
        try {
            $this->director()->remove('credentials/'.rawurlencode($id), DirectorClient::newKey());
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->is('owner_only') ? 'The Director does not let this credential revoke devices.' : $refused->explain();

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so nothing was revoked.';

            return;
        }

        $this->refresh();
    }
}
