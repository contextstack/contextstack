<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\IdentityStore;
use App\Client\Json;
use App\Native\DirectorScreen;
use App\Native\InviteQr;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Events\Biometric\Completed;
use Native\Mobile\Facades\Biometrics;

/**
 * The Director's members (CLIENT-API.md §2, "Member management and enrollment"), as this credential may manage them:
 * owners see everyone, admins members and viewers. Change a role, disable or reactivate a member, invite someone new
 * or reissue a pending member's invite, revoke an unspent invite, list and revoke a member's devices, and issue a
 * recovery pairing for a member. Every change first asks for this device's own unlock, as budget changes do.
 *
 * A new invite's or pairing's code, link and QR show once, in the answer that issued them, and are dropped when the
 * person taps Done or leaves the screen; nothing is written to the device.
 */
class Members extends DirectorScreen
{
    public const array Roles = ['owner', 'admin', 'member', 'viewer'];

    /** @var array<string, array<string, mixed>> member id => member */
    public array $members = [];

    /** @var list<array<string, mixed>> */
    public array $invites = [];

    /** @var list<array<string, mixed>> device credentials in the inventory this credential may see */
    public array $devices = [];

    public ?string $selected = null;

    public string $inviteName = '';

    public string $inviteRole = 'member';

    public string $recoveryName = '';

    /** The chosen member's display name as it is being edited. */
    public string $renameName = '';

    /**
     * The change waiting on the device unlock: its action (the Biometrics id), its target and value.
     *
     * @var array{action: string, target: string, value: string}|array{}
     */
    public array $pending = [];

    /**
     * A just-issued invite or recovery pairing, shown once.
     *
     * @var array{kind: string, name: string, code: string, link: string, expires_at: string}|array{}
     */
    public array $issued = [];

    public bool $loaded = false;

    /** This device's credential: it is never offered for revoking here (Settings forgets it instead). */
    public string $thisDevice = '';

    public function mount(): void
    {
        $this->thisDevice = (string) Json::nullableString(app(ConnectionStore::class)->saved()['credential_id'] ?? null);
        $this->refresh();
    }

    public function refresh(): void
    {
        $this->attempt(function (DirectorClient $director): void {
            $this->members = [];
            foreach (Json::records($director->get('members')['members'] ?? null) as $member) {
                $id = Json::string($member['id'] ?? null);
                if ($id !== '') {
                    $this->members[$id] = $member;
                }
            }
            $this->invites = Json::records($director->get('invites')['invites'] ?? null);
            $this->devices = $this->can('manage_member_devices')
                ? array_values(array_filter(Json::records($director->get('credentials')['credentials'] ?? null),
                    fn (array $credential): bool => ($credential['kind'] ?? null) === 'device'))
                : [];
            $this->loaded = true;
        });

        if ($this->selected !== null && ! isset($this->members[$this->selected])) {
            $this->selected = null;
        }
    }

    public function select(string $id): void
    {
        $this->selected = isset($this->members[$id]) ? $id : null;
        $this->recoveryName = '';
        $this->renameName = $this->selected !== null ? Json::string($this->members[$id]['display_name'] ?? null) : '';
        $this->problem = '';
    }

    public function closeDetail(): void
    {
        $this->selected = null;
    }

    /**
     * The roles a member's role may be changed to: an owner may give any role, an admin only member and viewer.
     *
     * @return list<string>
     */
    public function roleChoices(): array
    {
        $told = $this->identity()?->assignableRoles;
        if ($told !== null) {
            return $told;
        }

        return $this->identity()?->owner === true ? self::Roles : ['member', 'viewer'];
    }

    /**
     * The roles an invite may carry: never owner (the Director refuses it), and admin only from an owner.
     *
     * @return list<string>
     */
    public function inviteRoleChoices(): array
    {
        $told = $this->identity()?->invitableRoles;
        if ($told !== null) {
            return $told;
        }

        return $this->identity()?->owner === true ? ['admin', 'member', 'viewer'] : ['member', 'viewer'];
    }

    /**
     * @param  array<string, mixed>  $member
     * @return list<array<string, mixed>>
     */
    public function devicesOf(array $member): array
    {
        return array_values(array_filter($this->devices, fn (array $device): bool => ($device['member_id'] ?? null) === ($member['id'] ?? false)));
    }

    public static function roleWords(string $role): string
    {
        return ucfirst($role);
    }

    /**
     * @param  array<string, mixed>  $member
     */
    public static function summary(array $member): string
    {
        $status = Json::string($member['status'] ?? null);

        return self::roleWords(Json::string($member['role'] ?? null)).($status !== '' && $status !== 'active' ? ' · '.$status : '');
    }

    public function changeRole(string $memberId, string $role): void
    {
        if (isset($this->members[$memberId]) && in_array($role, $this->roleChoices(), true) && ($this->members[$memberId]['role'] ?? null) !== $role) {
            $this->ask('member-role', $memberId, $role);
        }
    }

    public function disableMember(string $memberId): void
    {
        if (isset($this->members[$memberId])) {
            $this->ask('member-status', $memberId, 'disabled');
        }
    }

    public function reactivateMember(string $memberId): void
    {
        if (isset($this->members[$memberId])) {
            $this->ask('member-status', $memberId, 'active');
        }
    }

    /**
     * Rename the member (`PATCH members/{id}` with `display_name` alone). Renaming someone else asks for the device
     * unlock like every other member change; renaming yourself does not, as in Settings, since it changes only your
     * own name. The Director checks the hierarchy.
     */
    public function renameMember(string $memberId): void
    {
        if (! isset($this->members[$memberId])) {
            return;
        }
        $name = trim($this->renameName);
        $invalid = self::displayNameProblem($name);
        if ($invalid !== '') {
            $this->problem = $invalid;

            return;
        }
        if ($name === Json::string($this->members[$memberId]['display_name'] ?? null)) {
            return;
        }
        if ($this->identity()?->isMe($memberId) === true) {
            $this->problem = '';
            $this->perform('member-rename', $memberId, $name);

            return;
        }
        $this->ask('member-rename', $memberId, $name);
    }

    /**
     * Why a display name would be refused (trimmed, 1–100 UTF-8 bytes, no control characters), or '' when it is fine.
     */
    public static function displayNameProblem(string $name): string
    {
        if ($name === '' || strlen($name) > 100) {
            return 'Enter a name of up to 100 bytes (fewer characters for accented letters or emoji).';
        }
        if (preg_match('/\p{Cc}/u', $name) !== 0) {
            return 'A name cannot contain line breaks, tabs or other control characters.';
        }

        return '';
    }

    public function invite(): void
    {
        $name = trim($this->inviteName);
        if ($name === '' || ! in_array($this->inviteRole, $this->inviteRoleChoices(), true)) {
            $this->problem = 'Enter the person\'s name and choose a role.';

            return;
        }
        $this->ask('invite', '', $name);
    }

    public function reissue(string $memberId): void
    {
        if (isset($this->members[$memberId])) {
            $this->ask('invite-reissue', $memberId, '');
        }
    }

    public function revokeInvite(string $inviteId): void
    {
        $this->ask('invite-revoke', $inviteId, '');
    }

    public function recoveryPairing(string $memberId): void
    {
        $name = trim($this->recoveryName);
        if (preg_match('/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/', $name) !== 1) {
            $this->problem = 'Name the new device with letters, numbers, dots, dashes or underscores, starting with a letter or number (for example ipad).';

            return;
        }
        $this->ask('recovery-pair', $memberId, $name);
    }

    public function revokeDevice(string $credentialId): void
    {
        if ($credentialId !== $this->thisDevice && in_array($credentialId, array_map(fn (array $device): string => Json::string($device['id'] ?? null), $this->devices), true)) {
            $this->ask('device-revoke', $credentialId, '');
        }
    }

    public function done(): void
    {
        $this->issued = [];
        $this->refresh();
    }

    /**
     * The device unlock answered. Only a success for the change that asked goes on to the Director.
     */
    #[On(Completed::class)]
    public function unlocked(bool $success, ?string $id = null): void
    {
        if ($this->pending === [] || $id !== $this->pending['action']) {
            return;
        }
        $pending = $this->pending;
        $this->pending = [];
        if (! $success) {
            $this->problem = 'Not unlocked, so nothing changed.';

            return;
        }

        $this->perform($pending['action'], $pending['target'], $pending['value']);
    }

    public function render(): View
    {
        $issuedQr = $this->issued !== [] && $this->issued['link'] !== '' ? InviteQr::dataUri($this->issued['link']) : '';

        return view('native.members', [
            'member' => $this->selected !== null ? ($this->members[$this->selected] ?? null) : null,
            'issuedQr' => $issuedQr,
        ]);
    }

    private function ask(string $action, string $target, string $value): void
    {
        $this->problem = '';
        $this->pending = ['action' => $action, 'target' => $target, 'value' => $value];
        Biometrics::prompt()->id($action)->prompt();
    }

    private function perform(string $action, string $target, string $value): void
    {
        $key = $action.':'.$target.':'.$value;

        try {
            match ($action) {
                'member-role' => $this->director()->put('members/'.rawurlencode($target), ['role' => $value], $this->keyFor($key), 'PATCH'),
                'member-status' => $this->director()->put('members/'.rawurlencode($target), ['status' => $value], $this->keyFor($key), 'PATCH'),
                'member-rename' => $this->director()->put('members/'.rawurlencode($target), ['display_name' => $value], $this->keyFor($key), 'PATCH'),
                'invite' => $this->show('invite', $value, $this->director()->write('invites', ['display_name' => $value, 'role' => $this->inviteRole], DirectorClient::newKey())->body),
                'invite-reissue' => $this->show('invite', Json::string($this->members[$target]['display_name'] ?? null), $this->director()->write('invites', ['member_id' => $target], DirectorClient::newKey())->body),
                'invite-revoke' => $this->director()->remove('invites/'.rawurlencode($target), $this->keyFor($key)),
                'recovery-pair' => $this->show('pairing', $value, $this->director()->write('pairings', ['name' => $value, 'member_id' => $target], $this->keyFor($key))->body),
                'device-revoke' => $this->director()->remove('credentials/'.rawurlencode($target), $this->keyFor($key)),
                default => null,
            };
        } catch (DirectorRefused $refused) {
            $this->answered($key);
            $words = $this->refused($refused);
            if ($refused->status !== 401) {
                $this->refresh();
                $this->problem = $refused->status === 409 ? 'The Director refused the change: '.$refused->detail : $words;
            }

            return;
        } catch (DirectorUnavailable) {
            if (in_array($action, ['invite', 'invite-reissue'], true)) {
                // An invite has no replay: asking again would make a second pending member or a second code.
                $this->refresh();
                $this->problem = 'The Director could not be reached, so the invite may have been created without reaching you. Check Invites below once the Director answers, and reissue it from there instead of inviting again.';

                return;
            }
            $this->problem = 'The Director could not be reached, so the change may not have arrived. Doing it again is safe.';

            return;
        }

        $this->answered($key);
        if ($action === 'invite') {
            $this->inviteName = '';
        }
        if ($action === 'recovery-pair') {
            $this->recoveryName = '';
        }
        if ($action === 'member-rename' && $this->identity()?->isMe($target) === true) {
            // `me` carries this member's name: read it again.
            app(IdentityStore::class)->forget();
        }
        $this->problem = '';
        $this->refresh();
    }

    /**
     * Keep a new code for this screen only, to show once.
     *
     * @param  array<string, mixed>  $answer
     */
    private function show(string $kind, string $name, array $answer): null
    {
        $code = Json::string($answer['code'] ?? null);
        // The Director's own link names the address it advertises; a Director from before it sent one for pairings
        // gets the contextstack://pair link built from this device's address.
        $link = Json::string($answer['link'] ?? null);
        if ($link === '' && $kind === 'pairing') {
            $link = 'contextstack://pair?'.http_build_query(['url' => $this->director()->connection->url, 'code' => $code]);
        }
        $this->issued = [
            'kind' => $kind,
            'name' => Json::string($answer['name'] ?? null, $name),
            'code' => $code,
            'link' => $link,
            'expires_at' => Json::string($answer['expires_at'] ?? null),
        ];

        return null;
    }
}
