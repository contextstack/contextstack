<?php

namespace App\Client;

/**
 * Who this device's credential acts for, from `GET me` (CLIENT-API.md §2): the member, the role and the capabilities
 * the Director advertises. Capabilities say eligibility only; the Director still checks each record, and its refusals
 * win. A Director from before teams sends no `capabilities`, and then every device is the owner's: `legacy` is true
 * and every action is offered, as before.
 */
final readonly class Identity
{
    /**
     * @param  list<string>|null  $capabilities  null when the Director does not send them (before teams)
     * @param  list<string>|null  $assignableRoles  roles this credential may give a member, when the Director says
     * @param  list<string>|null  $invitableRoles  roles this credential may invite with, when the Director says
     */
    public function __construct(
        public string $credentialId,
        public bool $owner,
        public ?string $memberId,
        public string $memberName,
        public ?string $role,
        public ?array $capabilities,
        public ?array $assignableRoles = null,
        public ?array $invitableRoles = null,
    ) {}

    /**
     * @param  array<string, mixed>  $me
     */
    public static function fromMe(array $me): self
    {
        $member = Json::record($me['member'] ?? null);
        $capabilities = is_array($me['capabilities'] ?? null) ? Json::strings($me['capabilities']) : null;

        return new self(
            credentialId: Json::string($me['credential_id'] ?? null),
            owner: ($me['owner'] ?? false) === true,
            memberId: Json::nullableString($member['id'] ?? null),
            memberName: Json::string($member['display_name'] ?? null),
            role: Json::nullableString($me['role'] ?? null),
            capabilities: $capabilities,
            assignableRoles: is_array($me['assignable_roles'] ?? null) ? Json::strings($me['assignable_roles']) : null,
            invitableRoles: is_array($me['invitable_roles'] ?? null) ? Json::strings($me['invitable_roles']) : null,
        );
    }

    public function legacy(): bool
    {
        return $this->capabilities === null;
    }

    /**
     * Whether the Director advertises this capability to this credential. Before teams, everything is offered.
     */
    public function can(string $capability): bool
    {
        return $this->capabilities === null || in_array($capability, $this->capabilities, true);
    }

    /**
     * Whether a record assigned to this member ID is this member's.
     */
    public function isMe(?string $memberId): bool
    {
        return $memberId !== null && $memberId !== '' && $memberId === $this->memberId;
    }

    public function roleWords(): string
    {
        return match ($this->role) {
            'owner' => 'Owner',
            'admin' => 'Admin',
            'member' => 'Member',
            'viewer' => 'Viewer',
            null => $this->owner ? 'Owner' : '',
            default => ucfirst($this->role),
        };
    }
}
