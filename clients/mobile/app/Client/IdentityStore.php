<?php

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Exceptions\TokenUnreadable;

/**
 * This launch's answer to `GET me`, read once and read again after pairing, when the app comes back, and when the
 * Director refuses something it used to allow (docs/design/teams-step2.md, "Both clients"). Kept in memory only: the
 * Director is the source of truth, and nothing about the member is written to the device.
 */
class IdentityStore
{
    private ?Identity $identity = null;

    private bool $tried = false;

    /** @var array<string, string>|null member ID => display name, for members this credential may list */
    private ?array $names = null;

    /** The status of the last failed read (401 when the Director no longer accepts this credential), or 0. */
    private int $failedStatus = 0;

    /**
     * Who this credential acts for, or null when the Director could not say just now (unreachable, a 5xx, not paired,
     * or the credential refused). Only a successful read is kept; a failed one is tried again after retry() (each
     * screen refresh) or forget().
     */
    public function current(): ?Identity
    {
        if ($this->tried) {
            return $this->identity;
        }
        $this->tried = true;

        try {
            $this->identity = Identity::fromMe(app(DirectorClient::class)->get('me'));
            $this->failedStatus = 0;
        } catch (DirectorRefused $refused) {
            $this->failedStatus = $refused->status;
        } catch (DirectorUnavailable|NotConnected|TokenUnreadable) {
            $this->failedStatus = 0;
        }

        return $this->identity;
    }

    /**
     * Whether the current identity allows the capability. A Director that answered `me` without capabilities predates
     * teams and allows everything; while `me` is unknown the screens are read-only. The Director still decides each
     * request.
     */
    public function can(string $capability): bool
    {
        return $this->current()?->can($capability) ?? false;
    }

    /**
     * A screen is refreshing: if the last read of `me` failed, try it again.
     */
    public function retry(): void
    {
        if ($this->identity === null) {
            $this->tried = false;
        }
    }

    /**
     * Display names for member IDs: `GET members` for a credential that manages members, otherwise the member
     * directory every active member may read (`GET members/directory`). A Director without the directory answers
     * 404, and other members then show by ID.
     *
     * @return array<string, string>
     */
    public function memberNames(): array
    {
        if ($this->names !== null) {
            return $this->names;
        }
        $identity = $this->current();
        $this->names = [];
        if ($identity?->memberId !== null) {
            $this->names[$identity->memberId] = $identity->memberName;
        }
        if ($identity === null || $identity->legacy()) {
            return $this->names;
        }
        $path = $identity->can('manage_members') ? 'members' : 'members/directory';

        try {
            $answer = app(DirectorClient::class)->get($path);
            // The directory may answer a bare list or {members: [...]}.
            foreach (Json::records(isset($answer['members']) ? $answer['members'] : array_values($answer)) as $member) {
                $id = Json::string($member['id'] ?? null);
                if ($id !== '') {
                    $this->names[$id] = Json::string($member['display_name'] ?? null, $id);
                }
            }
        } catch (DirectorRefused|DirectorUnavailable|NotConnected|TokenUnreadable) {
            // Names are a convenience; IDs still show.
        }

        return $this->names;
    }

    public function unauthenticated(): bool
    {
        $this->current();

        return $this->failedStatus === 401;
    }

    /**
     * Read `me` again on next use: after pairing, on return to the app, on a 401, 403 or 404, or on forgetting.
     */
    public function forget(): void
    {
        $this->identity = null;
        $this->names = null;
        $this->tried = false;
        $this->failedStatus = 0;
    }
}
