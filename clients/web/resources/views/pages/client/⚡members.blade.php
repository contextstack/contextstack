<?php

use App\Client\AdvertisedUrl;
use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Identity;
use App\Client\QrCode;
use App\Client\TailscaleAddress;
use App\Livewire\DirectorPage;
use Flux\Flux;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;

/*
 * Members and invites (CLIENT-API.md §2, Member management and enrollment), for an owner or admin (manage_members).
 * The Director lists only the members this device may manage. A new invite's code, link and QR are shown once, in
 * the answer to issuing it. They are never component state (wire:snapshot travels with every request), the session,
 * the cache or a URL: the answer dispatches them once to a browser-only box (cs-secret-issued), which holds them
 * until Done or leaving the page, and empties itself before wire:navigate keeps a copy of the page for Back.
 * The Director writes the link: from the owner's advertised URL when one is set, else from the address another device
 * uses, which this desktop sends with each code (X-ContextStack-Invite-Origin, honoured only from loopback). Reissuing
 * gives a pending member a fresh code. A recovery pairing code lets an active member pair a new device. Anyone renames
 * their own member record here; a manager renames the members they manage (PATCH members/{id} {display_name}).
 */
new #[Layout('layouts::client')] #[Title('Members')] class extends DirectorPage
{
    /** @var list<array{id: string, display_name: string, role: string, status: string}> */
    #[Locked]
    public array $members = [];

    /** @var list<array<string, mixed>> invite metadata, never codes */
    #[Locked]
    public array $invites = [];

    #[Locked]
    public bool $allowed = true;

    #[Locked]
    public string $address = '';

    /** The owner-set advertised URL when this device may read it (AdvertisedUrl), else null. */
    #[Locked]
    public ?string $advertised = null;

    /** The member being renamed in the rename modal. */
    #[Locked]
    public string $renameMember = '';

    public string $renameName = '';

    public string $inviteName = '';

    public string $inviteRole = 'member';

    public string $recoveryMember = '';

    public string $recoveryName = '';

    public function mount(ConnectionStore $connections, TailscaleAddress $tailscale): void
    {
        try {
            $this->address = $tailscale->directorAddress($connections->resolve());
        } catch (NotConnected) {
            $this->address = '';
        }
        $this->advertised = app(AdvertisedUrl::class)->read();
        $this->refetch();
    }

    public function refetch(): void
    {
        $this->allowed = app(Identity::class)->can('manage_members');
        if (! $this->allowed) {
            $this->members = [];
            $this->invites = [];

            return;
        }

        $members = $this->attempt(fn () => $this->director()->get('members'));
        if ($members === null) {
            return;
        }
        $invites = $this->attempt(fn () => $this->director()->get('invites')) ?? ['invites' => []];

        $this->members = [];
        foreach ((array) ($members['members'] ?? []) as $member) {
            if (is_array($member) && is_string($member['id'] ?? null)) {
                $this->members[] = [
                    'id' => $member['id'],
                    'display_name' => (string) ($member['display_name'] ?? $member['id']),
                    'role' => (string) ($member['role'] ?? ''),
                    'status' => (string) ($member['status'] ?? ''),
                ];
            }
        }
        $this->invites = array_values(array_filter((array) ($invites['invites'] ?? []), fn ($invite): bool => is_array($invite) && is_string($invite['id'] ?? null)));
    }

    protected function follows(array $change): bool
    {
        return false;
    }

    protected function signOut(): void
    {
        $this->members = [];
        $this->invites = [];
        $this->dispatch('cs-secret-cleared');
        parent::signOut();
    }

    /**
     * Roles this device can give: the Director's `me.invitable_roles` or `me.assignable_roles` when it sends them;
     * otherwise an owner gives any (owner only by role change) and an admin manages members and viewers.
     *
     * @return list<string>
     */
    public function roles(bool $forInvite = false): array
    {
        $identity = app(Identity::class);
        $advertised = $identity->me()[$forInvite ? 'invitable_roles' : 'assignable_roles'] ?? null;
        if (is_array($advertised)) {
            return array_values(array_filter($advertised, fn ($role): bool => is_string($role) && $role !== ''));
        }

        $roles = $identity->role() === 'admin' ? ['member', 'viewer'] : ['admin', 'member', 'viewer'];
        if (! $forInvite && $identity->role() === 'owner') {
            array_unshift($roles, 'owner');
        }

        return $roles;
    }

    public function invite(DirectorClient $director): void
    {
        $this->validate([
            'inviteName' => ['required', 'string', 'max:100'],
            'inviteRole' => ['required', 'in:'.implode(',', $this->roles(true))],
        ]);

        if ($this->issue($director, ['display_name' => trim($this->inviteName), 'role' => $this->inviteRole], trim($this->inviteName))) {
            $this->inviteName = '';
        }
    }

    public function reissue(DirectorClient $director, string $memberId): void
    {
        $member = $this->member($memberId);
        if ($member === null || $member['status'] !== 'pending') {
            return;
        }

        $this->issue($director, ['member_id' => $memberId], $member['display_name']);
    }

    public function changeRole(DirectorClient $director, string $memberId, string $role): void
    {
        if ($this->member($memberId) === null || ! in_array($role, $this->roles(), true)) {
            return;
        }

        $this->patch($director, $memberId, ['role' => $role], __('Role changed.'));
    }

    public function setStatus(DirectorClient $director, string $memberId, string $status): void
    {
        if ($this->member($memberId) === null || ! in_array($status, ['active', 'disabled'], true)) {
            return;
        }

        $this->patch($director, $memberId, ['status' => $status], $status === 'disabled'
            ? __('Disabled. Their devices and invites stop working now.')
            : __('Reactivated. Issue a recovery pairing code so they can pair a device again.'));
    }

    /**
     * Open the rename modal for a member: this device's own record always, another member's when this device manages
     * members (the Director still checks the hierarchy).
     */
    public function startRename(string $memberId): void
    {
        $current = $this->renameable($memberId);
        if ($current === null) {
            return;
        }

        $this->resetValidation('renameName');
        $this->renameMember = $memberId;
        $this->renameName = $current;
        Flux::modal('rename-member')->show();
    }

    public function rename(DirectorClient $director): void
    {
        if ($this->renameable($this->renameMember) === null) {
            return;
        }

        $name = trim($this->renameName);
        $this->renameName = $name;
        $this->validate(['renameName' => ['required', 'string', function (string $attribute, string $value, Closure $fail): void {
            if (strlen($value) > 100) {
                $fail(__('Use at most 100 bytes.'));
            } elseif (preg_match('/\p{Cc}/u', $value) === 1) {
                $fail(__('Use a name without control characters.'));
            }
        }]]);

        $self = app(Identity::class)->isMe($this->renameMember);
        $this->patch($director, $this->renameMember, ['display_name' => $name], __('Renamed to :name.', ['name' => $name]));
        if ($this->problem === '') {
            Flux::modal('rename-member')->close();
            if ($self) {
                app(Identity::class)->forget();
            }
        }
    }

    /**
     * The current name of a member this device may rename, or null.
     */
    public function renameable(string $memberId): ?string
    {
        $identity = app(Identity::class);
        if ($identity->isMe($memberId) && ! is_string($identity->me()['conversation_id'] ?? null)) {
            return (string) ($this->member($memberId)['display_name'] ?? $identity->displayName() ?? '');
        }

        return $this->allowed ? $this->member($memberId)['display_name'] ?? null : null;
    }

    public function cancelInvite(DirectorClient $director, string $inviteId): void
    {
        if (! in_array($inviteId, array_column($this->invites, 'id'), true)) {
            return;
        }

        $this->act(fn () => $director->remove('invites/'.rawurlencode($inviteId), DirectorClient::newKey()), __('Invite cancelled.'));
    }

    /**
     * A one-time pairing code for an active member's new device (`POST pairings` with `member_id`).
     */
    public function recover(DirectorClient $director): void
    {
        $this->validate([
            'recoveryMember' => ['required', 'in:'.implode(',', array_column(array_filter($this->members, fn (array $m): bool => $m['status'] === 'active'), 'id'))],
            'recoveryName' => ['required', 'string', 'max:100', 'regex:/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/'],
        ], [
            'recoveryName.regex' => __('Use letters, numbers, dots, dashes or underscores, starting with a letter or number (for example ipad).'),
        ]);

        $answer = $this->attempt(fn () => $director->issuePairing(['name' => $this->recoveryName, 'member_id' => $this->recoveryMember], DirectorClient::newKey(), $this->origin()));
        if ($answer === null) {
            return;
        }

        $code = (string) ($answer->body['code'] ?? '');
        $this->reveal('pairing', (string) ($this->member($this->recoveryMember)['display_name'] ?? $this->recoveryName), $code,
            AdvertisedUrl::link('pair', $answer->body['link'] ?? null, $this->advertised ?? $this->address, $code), (string) ($answer->body['expires_at'] ?? ''));
        $this->recoveryName = '';
        $this->recoveryMember = '';
    }

    /**
     * Hand a just-issued code to the browser once (CLIENT-API.md §2: codes stay out of history and storage). Nothing
     * of it is kept here, so no later request carries it.
     */
    private function reveal(string $kind, string $name, string $code, string $link, string $expiresAt): void
    {
        $this->dispatch('cs-secret-issued',
            heading: $kind === 'invite' ? __('Invite for :name', ['name' => $name]) : __('Pairing code for :name', ['name' => $name]),
            codeLabel: $kind === 'invite' ? __('Invite code') : __('Pairing code'),
            code: $code,
            address: AdvertisedUrl::address($link) ?: ($this->advertised ?? $this->address),
            link: $link,
            qr: $link !== '' ? QrCode::svg($link) : '',
            until: $expiresAt,
            hint: $kind === 'invite' ? __('They open ContextStack, choose Join with an invite, and paste the link or code. It works once.') : __('It works once.'),
        );
    }

    /**
     * @return array{id: string, display_name: string, role: string, status: string}|null
     */
    private function member(string $memberId): ?array
    {
        foreach ($this->members as $member) {
            if ($member['id'] === $memberId) {
                return $member;
            }
        }

        return null;
    }

    /**
     * The address to send as X-ContextStack-Invite-Origin: none when the owner set an advertised URL (the Director uses
     * that), else this desktop's address for other devices.
     */
    private function origin(): ?string
    {
        return $this->advertised === null && $this->address !== '' ? $this->address : null;
    }

    /**
     * @param  array<string, mixed>  $body
     */
    private function issue(DirectorClient $director, array $body, string $name): bool
    {
        if ($this->advertised === null && $this->address === '') {
            $this->problem = __('This Director has no address another device can reach (no Tailscale address was found). Connect Tailscale, or have the owner set the Director\'s address in Settings, and try again.');

            return false;
        }

        $answer = $this->attempt(fn () => $director->issueInvite($body, $this->origin()));
        if ($answer === null) {
            return false;
        }

        $code = (string) ($answer->body['code'] ?? '');
        $this->reveal('invite', $name, $code, AdvertisedUrl::link('invite', $answer->body['link'] ?? null, $this->advertised ?? $this->address, $code), (string) ($answer->body['expires_at'] ?? ''));
        $this->refetch();

        return true;
    }

    /**
     * @param  array<string, string>  $body
     */
    private function patch(DirectorClient $director, string $memberId, array $body, string $done): void
    {
        $this->act(fn () => $director->put('members/'.rawurlencode($memberId), $body, DirectorClient::newKey(), 'PATCH'), $done);
    }

    private function act(callable $call, string $done): void
    {
        try {
            $call();
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->status === 409
                ? __('The Director kept this as it was: an organisation keeps at least one active owner, you cannot change your own role or status here, and a pending member joins only through an invite.')
                : $refused->explain();
            $this->refused($refused);

            return;
        } catch (DirectorUnavailable|NotConnected $failure) {
            $this->problem = $failure->getMessage();

            return;
        }

        $this->problem = '';
        Flux::toast(variant: 'success', text: $done);
        $this->refetch();
    }
}; ?>

<section class="mx-auto flex w-full max-w-4xl flex-col gap-6" data-test="members-page">
    <div>
        <flux:heading size="lg" level="1" class="text-lg!">{{ __('Members') }}</flux:heading>
        <flux:text class="mt-0.5">{{ __('People who use this Director, their roles, and invites for new people. Each person pairs their own devices.') }}</flux:text>
    </div>

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    @if (! $allowed)
        <flux:callout icon="information-circle" data-test="members-not-allowed">
            <flux:callout.text>{{ __('Your role does not manage members. An owner or admin can invite people and change roles.') }}</flux:callout.text>
        </flux:callout>
        @php($myId = app(\App\Client\Identity::class)->memberId())
        @if ($myId !== null && $this->renameable($myId) !== null)
            <flux:card class="flex flex-wrap items-center justify-between gap-3" data-test="own-name">
                <div>
                    <flux:heading size="lg" level="2">{{ __('Your name') }}</flux:heading>
                    <flux:text class="mt-1">{{ $this->renameable($myId) }}</flux:text>
                </div>
                <flux:button size="sm" wire:click="startRename('{{ $myId }}')" data-test="rename-self">{{ __('Rename') }}</flux:button>
            </flux:card>
        @endif
    @elseif (! $signedOut)
        {{--
            The code just issued lives only here, in the browser: it arrives once with cs-secret-issued and goes on
            Done, when the page signs out (cs-secret-cleared), and before wire:navigate keeps a copy of the page for Back
            (livewire:navigating fires before that copy is taken).
        --}}
        <div wire:ignore x-data="{ secret: null }" x-ref="secretBox"
             x-on:cs-secret-issued.window="secret = $event.detail"
             x-on:cs-secret-cleared.window="secret = null"
             x-init="document.addEventListener('livewire:navigating', () => { secret = null; $el.replaceChildren() }, { once: true })"
             data-test="secret-box">
            <template x-if="secret">
                <flux:card class="flex flex-col gap-3" data-test="issued-secret" data-secret>
                    <flux:heading size="lg" level="2" x-text="secret.heading"></flux:heading>
                    <flux:text>{{ __('Shown only now: copy it or let them scan it before you leave this page.') }}</flux:text>
                    <div class="flex flex-wrap items-center gap-6">
                        <template x-if="secret.qr">
                            <div class="rounded-lg bg-white p-3" aria-label="{{ __('QR code') }}" x-html="secret.qr"></div>
                        </template>
                        <dl class="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2">
                            <dt class="text-sm text-zinc-500">{{ __('Director address') }}</dt>
                            <dd class="font-mono" x-text="secret.address"></dd>
                            <dt class="text-sm text-zinc-500" x-text="secret.codeLabel"></dt>
                            <dd class="font-mono text-xl tracking-wider select-all break-all" x-text="secret.code"></dd>
                            <template x-if="secret.link">
                                <dt class="text-sm text-zinc-500">{{ __('Link') }}</dt>
                            </template>
                            <template x-if="secret.link">
                                <dd class="font-mono text-sm select-all break-all" x-text="secret.link"></dd>
                            </template>
                        </dl>
                    </div>
                    <flux:text class="text-sm"><span x-text="secret.hint"></span> {{ __('Until') }} <span x-text="secret.until"></span>.</flux:text>
                    <div><flux:button size="sm" x-on:click="secret = null">{{ __('Done') }}</flux:button></div>
                </flux:card>
            </template>
        </div>

        <flux:card class="flex flex-col gap-3">
            <flux:heading size="lg" level="2">{{ __('Invite someone') }}</flux:heading>
            <form wire:submit="invite" class="flex flex-wrap items-end gap-3">
                <flux:input wire:model="inviteName" :label="__('Name')" class="min-w-48 flex-1" data-test="invite-name" />
                <flux:select wire:model="inviteRole" :label="__('Role')" class="max-w-40">
                    @foreach ($this->roles(true) as $role)
                        <flux:select.option :value="$role">{{ __(ucfirst($role)) }}</flux:select.option>
                    @endforeach
                </flux:select>
                <flux:button type="submit" variant="primary">{{ __('Create invite') }}</flux:button>
            </form>
        </flux:card>

        <div class="flex flex-col gap-2">
            <flux:heading size="lg" level="2">{{ __('People') }}</flux:heading>
            @forelse ($members as $member)
                <div class="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-zinc-200 px-4 py-3 dark:border-zinc-700" wire:key="member-{{ $member['id'] }}" data-test="member-row">
                    <div class="min-w-0">
                        <div class="font-medium">{{ $member['display_name'] }}</div>
                        <div class="text-sm text-zinc-500">
                            {{ __(ucfirst($member['role'])) }} · {{ match ($member['status']) { 'pending' => __('invited, not joined yet'), 'disabled' => __('disabled'), default => __('active') } }}
                        </div>
                    </div>
                    @if (app(\App\Client\Identity::class)->isMe($member['id']))
                        <div class="flex flex-wrap items-center gap-2">
                            <flux:badge size="sm">{{ __('You') }}</flux:badge>
                            @if ($this->renameable($member['id']) !== null)
                            <flux:button size="sm" variant="ghost" wire:click="startRename('{{ $member['id'] }}')" data-test="rename">{{ __('Rename') }}</flux:button>
                            @endif
                        </div>
                    @else
                        <div class="flex flex-wrap items-center gap-2">
                            <flux:button size="sm" variant="ghost" wire:click="startRename('{{ $member['id'] }}')" data-test="rename">{{ __('Rename') }}</flux:button>
                            @if ($member['status'] === 'pending')
                                <flux:button size="sm" wire:click="reissue('{{ $member['id'] }}')" data-test="reissue">{{ __('New invite code') }}</flux:button>
                            @else
                                <flux:dropdown>
                                    <flux:button size="sm" icon:trailing="chevron-down" data-test="change-role">{{ __('Role') }}</flux:button>
                                    <flux:menu>
                                        @foreach ($this->roles() as $role)
                                            @if ($role !== $member['role'])
                                                <flux:menu.item wire:click="changeRole('{{ $member['id'] }}', '{{ $role }}')">{{ __('Make :role', ['role' => __($role)]) }}</flux:menu.item>
                                            @endif
                                        @endforeach
                                    </flux:menu>
                                </flux:dropdown>
                                @if ($member['status'] === 'disabled')
                                    <flux:button size="sm" wire:click="setStatus('{{ $member['id'] }}', 'active')" data-test="reactivate">{{ __('Reactivate') }}</flux:button>
                                @else
                                    <flux:button size="sm" variant="danger" wire:click="setStatus('{{ $member['id'] }}', 'disabled')" data-test="disable"
                                        wire:confirm="{{ __('Disable :name? Their devices stop working at once and their scheduled work pauses until someone takes it over.', ['name' => $member['display_name']]) }}">
                                        {{ __('Disable') }}
                                    </flux:button>
                                @endif
                            @endif
                        </div>
                    @endif
                </div>
            @empty
                <flux:text>{{ __('No members you can manage yet.') }}</flux:text>
            @endforelse
        </div>

        @php($pending = array_values(array_filter($invites, fn (array $invite): bool => ($invite['status'] ?? '') === 'pending')))
        <div class="flex flex-col gap-2" data-test="invites">
            <flux:heading size="lg" level="2">{{ __('Open invites') }}</flux:heading>
            @forelse ($pending as $invite)
                @php($who = collect($members)->firstWhere('id', $invite['member_id'] ?? null))
                <div class="flex items-center justify-between gap-3 rounded-lg border border-zinc-200 px-4 py-3 dark:border-zinc-700" wire:key="invite-{{ $invite['id'] }}" data-test="invite-row">
                    <div class="min-w-0 text-sm">
                        <span class="font-medium">{{ $who['display_name'] ?? $invite['member_id'] ?? '' }}</span>
                        · {{ __(ucfirst((string) ($invite['role'] ?? ''))) }}
                        · {{ __('until') }} <x-client.time :at="(string) ($invite['expires_at'] ?? '')" />
                    </div>
                    <flux:button size="sm" variant="ghost" wire:click="cancelInvite('{{ $invite['id'] }}')" data-test="cancel-invite">{{ __('Cancel') }}</flux:button>
                </div>
            @empty
                <flux:text>{{ __('No open invites.') }}</flux:text>
            @endforelse
        </div>

        @php($active = array_values(array_filter($members, fn (array $member): bool => $member['status'] === 'active' && ! app(\App\Client\Identity::class)->isMe($member['id']))))
        @if ($active !== [])
            <flux:card class="flex flex-col gap-3" data-test="recovery">
                <flux:heading size="lg" level="2">{{ __('Pair a device for someone') }}</flux:heading>
                <flux:text>{{ __('For a member who lost their device: a one-time code that pairs a new device as them.') }}</flux:text>
                <form wire:submit="recover" class="flex flex-wrap items-end gap-3">
                    <flux:select wire:model="recoveryMember" :label="__('Member')" class="max-w-56" :placeholder="__('Choose…')">
                        @foreach ($active as $member)
                            <flux:select.option :value="$member['id']">{{ $member['display_name'] }}</flux:select.option>
                        @endforeach
                    </flux:select>
                    <flux:input wire:model="recoveryName" :label="__('Device name')" placeholder="ipad" class="max-w-48" />
                    <flux:button type="submit">{{ __('Get a pairing code') }}</flux:button>
                </form>
            </flux:card>
        @endif
    @endif

    <flux:modal name="rename-member" class="md:w-96" data-test="rename-modal">
        <form wire:submit="rename" class="flex flex-col gap-4">
            <flux:heading size="lg">{{ __('Rename') }}</flux:heading>
            <flux:input wire:model="renameName" :label="__('Name')" maxlength="100" data-test="rename-name" />
            @if ($problem !== '')
                <flux:text class="text-red-600 dark:text-red-400" data-test="rename-problem">{{ $problem }}</flux:text>
            @endif
            <div class="flex justify-end gap-2">
                <flux:modal.close>
                    <flux:button variant="ghost">{{ __('Cancel') }}</flux:button>
                </flux:modal.close>
                <flux:button type="submit" variant="primary">{{ __('Save') }}</flux:button>
            </div>
        </form>
    </flux:modal>
</section>
