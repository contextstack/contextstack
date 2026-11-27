<?php

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;

/**
 * Member IDs as people's names, for the sponsor of a conversation, the member responsible for work and the assignee
 * of a needs-you item. A record's own `*_display_name` comes first. Otherwise names come from `GET members` when
 * this device may list members (manage_members), else from `GET members/directory` (every active member may read it;
 * a Director without it answers 404); this device's own member comes from `me`. Any other ID is shown compactly,
 * never guessed at.
 */
class MemberNames
{
    /** @var array<string, string>|null */
    private ?array $names = null;

    /** @var array<string, string> member id => role, for the members listed */
    private array $roles = [];

    public function __construct(private readonly Identity $identity) {}

    /**
     * Words for one member ID: "you", the record's display name, a listed name, or a shortened ID. Null for no ID.
     */
    public function label(mixed $memberId, mixed $displayName = null): ?string
    {
        if (! is_string($memberId) || $memberId === '') {
            return null;
        }

        if ($this->identity->isMe($memberId)) {
            return __('you');
        }

        if (is_string($displayName) && $displayName !== '') {
            return $displayName;
        }

        return $this->names()[$memberId] ?? self::compact($memberId);
    }

    /**
     * The members this device may manage, id => display name ([] when it may not list them).
     *
     * @return array<string, string>
     */
    public function names(): array
    {
        if ($this->names !== null) {
            return $this->names;
        }

        $this->names = [];
        if (! is_array($this->identity->me()['capabilities'] ?? null)) {
            return $this->names;
        }

        try {
            $director = app(DirectorClient::class);
            $members = $this->identity->can('manage_members')
                ? $director->get('members')['members'] ?? []
                : $director->find('members/directory') ?? [];
        } catch (DirectorRefused|DirectorUnavailable|NotConnected) {
            return $this->names;
        }
        // The directory answers a bare list; `members` wraps it.
        if (is_array($members) && ! array_is_list($members)) {
            $members = $members['members'] ?? [];
        }

        foreach (is_array($members) ? $members : [] as $member) {
            if (is_array($member) && is_string($member['id'] ?? null) && is_string($member['display_name'] ?? null)) {
                $this->names[$member['id']] = $member['display_name'];
                $this->roles[$member['id']] = is_string($member['role'] ?? null) ? $member['role'] : '';
            }
        }

        return $this->names;
    }

    /**
     * A member's role: this device's own from `me`, another's from `GET members`; null when not known.
     */
    public function role(mixed $memberId): ?string
    {
        if (! is_string($memberId) || $memberId === '') {
            return null;
        }
        if ($this->identity->isMe($memberId)) {
            return $this->identity->role();
        }
        $this->names();

        return ($this->roles[$memberId] ?? '') !== '' ? $this->roles[$memberId] : null;
    }

    public static function compact(string $memberId): string
    {
        return mb_strlen($memberId) > 14 ? mb_substr($memberId, 0, 12).'…' : $memberId;
    }
}
