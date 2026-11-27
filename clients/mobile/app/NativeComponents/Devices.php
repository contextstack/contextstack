<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\DirectorScreen;
use App\Native\InviteQr;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * This member's devices on the Director (`GET credentials` with `member_id`, teams step 2): list them, pair another one, revoke one. Pairing and revoking first ask for this
 * device's own unlock (iOS device-owner authentication: Face ID, Touch ID or the passcode, whichever the owner set
 * up), so a lost but unlocked-once phone cannot hand out access by itself. This device cannot revoke itself here;
 * Settings forgets it instead. An owner's or admin's inventory also lists other members' devices; managing those stays
 * in Members, so they are counted here, not listed. A Director from before teams lists every device, as before.
 */
class Devices extends DirectorScreen
{
    /** @var list<array<string, mixed>> */
    public array $devices = [];

    public string $thisDevice = '';

    public string $name = '';

    /**
     * A just-issued pairing code, and the Director's `contextstack://pair` link for it when it sends one ('' before).
     *
     * @var array{code: string, name: string, expires_at: string, link: string}|array{}
     */
    public array $issued = [];

    public bool $allowed = true;

    /** Other members' devices in the inventory, managed from Members. */
    public int $others = 0;

    public bool $loaded = false;

    public function mount(): void
    {
        $credentialId = Json::nullableString(app(ConnectionStore::class)->saved()['credential_id'] ?? null);
        $this->thisDevice = (string) $credentialId;
        $this->refresh();
    }

    public function refresh(): void
    {
        try {
            $credentials = Json::records($this->director()->get('credentials')['credentials'] ?? null);
        } catch (DirectorRefused $refused) {
            if ($refused->status === 401) {
                $this->refused($refused);

                return;
            }
            $this->allowed = ! $refused->is('owner_only') && ! $refused->is('forbidden');
            $this->problem = $this->allowed ? $refused->explain() : '';
            $this->loaded = true;

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached. Check that Tailscale is on, then try again.';

            return;
        }

        $this->allowed = true;
        $this->problem = '';
        $devices = array_values(array_filter($credentials, fn (array $credential): bool => ($credential['kind'] ?? null) === 'device'));
        $me = $this->identity()?->memberId;
        $this->devices = $me === null ? $devices : array_values(array_filter($devices,
            fn (array $device): bool => Json::nullableString($device['member_id'] ?? null) === null || $device['member_id'] === $me));
        $this->others = count($devices) - count($this->devices);
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

    /**
     * Whose device it is, in words: this member's (with the role), or the member's name before teams said otherwise.
     *
     * @param  array<string, mixed>  $device
     */
    public function owner(array $device): string
    {
        $identity = $this->identity();
        if ($identity === null || $identity->memberId === null) {
            return '';
        }

        return trim('Yours · '.$identity->roleWords(), ' ·');
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
        return view('native.devices', [
            'address' => app(ConnectionStore::class)->url(),
            'issuedQr' => ($this->issued['link'] ?? '') !== '' ? InviteQr::dataUri($this->issued['link']) : '',
        ]);
    }

    private function issue(): void
    {
        try {
            $answer = $this->director()->write('pairings', ['name' => $this->name], $this->keyFor('pair:'.$this->name));
        } catch (DirectorRefused $refused) {
            $this->answered('pair:'.$this->name);
            $this->problem = $refused->is('owner_only') ? 'The Director does not let this credential pair devices.' : $refused->explain();

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the code may have been issued without reaching you. Asking again is safe: it shows the same code.';

            return;
        }

        $this->answered('pair:'.$this->name);

        $this->problem = '';
        $this->issued = [
            'code' => Json::string($answer->body['code'] ?? null),
            'name' => Json::string($answer->body['name'] ?? null, $this->name),
            'expires_at' => Json::string($answer->body['expires_at'] ?? null),
            'link' => Json::string($answer->body['link'] ?? null),
        ];
        $this->name = '';
    }

    private function remove(string $id): void
    {
        try {
            $this->director()->remove('credentials/'.rawurlencode($id), $this->keyFor('revoke:'.$id));
        } catch (DirectorRefused $refused) {
            $this->answered('revoke:'.$id);
            $this->problem = $refused->is('owner_only') ? 'The Director does not let this credential revoke devices.' : $refused->explain();

            return;
        } catch (DirectorUnavailable) {
            $this->problem = 'The Director could not be reached, so the revoke may not have arrived. Revoking again is safe.';

            return;
        }

        $this->answered('revoke:'.$id);

        $this->refresh();
    }
}
