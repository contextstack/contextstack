<?php

use App\Client\AdvertisedUrl;
use App\Client\Connection;
use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Identity;
use App\Client\MemberNames;
use App\Client\TailscaleAddress;
use Flux\Flux;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Component;

/*
 * Devices paired with this Director (CLIENT-API.md §2): issue a one-time pairing code for a phone, iPad or another
 * computer, list the paired devices, and revoke one. Every member pairs and revokes their own devices; the Director
 * also lists an owner's or admin's view of other members' devices (manage_member_devices), each with its member and
 * role. A lead's credential gets 403 owner_only, and the page says so. This device never revokes itself here (the
 * Director refuses with 409 self_revoke): Settings forgets it. The QR shows the Director's own `contextstack://pair`
 * link: from the owner's advertised URL when set, else from the address this desktop sends (AdvertisedUrl).
 */
new #[Layout('layouts::client')] #[Title('Devices')] class extends Component
{
    public string $name = '';

    /** @var list<array<string, mixed>> device credentials, newest first */
    #[Locked]
    public array $devices = [];

    /** @var array{code: string, name: string, expires_at: string, link: string}|array{} the code just issued */
    #[Locked]
    public array $issued = [];

    #[Locked]
    public bool $owner = true;

    /** This device's own credential id, when it is a paired device. */
    #[Locked]
    public string $self = '';

    #[Locked]
    public string $address = '';

    /** The owner-set advertised URL when this device may read it, else null. */
    #[Locked]
    public ?string $advertised = null;

    public string $problem = '';

    public function mount(ConnectionStore $connections, TailscaleAddress $tailscale): void
    {
        try {
            $connection = $connections->resolve();
        } catch (NotConnected) {
            $this->redirect(route('connect'), navigate: true);

            return;
        }

        $this->self = $connection->mode === Connection::Device ? (string) $connection->credentialId : '';
        $this->address = $tailscale->directorAddress($connection);
        $this->advertised = app(AdvertisedUrl::class)->read();
        $this->load();
    }

    public function pair(DirectorClient $director): void
    {
        $this->problem = '';
        $this->validate(['name' => ['required', 'string', 'max:100', 'regex:/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/']], [
            'name.regex' => __('Use letters, numbers, dots, dashes or underscores, starting with a letter or number (for example ipad or office-ipad).'),
        ]);

        try {
            $answer = $director->issuePairing(['name' => $this->name], DirectorClient::newKey(),
                $this->advertised === null && $this->address !== '' ? $this->address : null);
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();

            return;
        }

        $code = (string) ($answer->body['code'] ?? '');
        $this->issued = [
            'code' => $code,
            'name' => (string) ($answer->body['name'] ?? $this->name),
            'expires_at' => (string) ($answer->body['expires_at'] ?? ''),
            'link' => AdvertisedUrl::link('pair', $answer->body['link'] ?? null, $this->advertised ?? $this->address, $code),
        ];
        $this->name = '';
    }

    public function revoke(DirectorClient $director, string $id): void
    {
        $this->problem = '';

        if ($id === $this->self || ! in_array($id, array_column($this->devices, 'id'), true)) {
            return;
        }

        try {
            $director->remove('credentials/'.rawurlencode($id), DirectorClient::newKey());
        } catch (DirectorRefused $refused) {
            $this->problem = match (true) {
                $refused->is('self_revoke') => __('A device cannot revoke itself. Use Forget this device in Settings.'),
                $refused->is('protected_credential') => __('The Director\'s own local credential cannot be revoked from here.'),
                default => $refused->explain(),
            };
            if (in_array($refused->status, [401, 403, 404], true)) {
                app(Identity::class)->forget();
                $this->load();
            }

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();

            return;
        }

        Flux::toast(variant: 'success', text: __('Revoked. That device can no longer reach the Director.'));
        $this->load();
    }

    /**
     * Who a device belongs to: "you", or the member's name and role when this device may see them.
     *
     * @param  array<string, mixed>  $device
     */
    public function owner(array $device): ?string
    {
        $names = app(MemberNames::class);
        $label = $names->label($device['member_id'] ?? null);
        if ($label === null) {
            return null;
        }
        $role = $names->role($device['member_id'] ?? null);

        return $role !== null ? $label.' · '.__(ucfirst($role)) : $label;
    }

    /**
     * Whether the device is this member's own (or the Director predates members).
     *
     * @param  array<string, mixed>  $device
     */
    public function mine(array $device): bool
    {
        $memberId = $device['member_id'] ?? null;

        return ! is_string($memberId) || app(MemberNames::class)->label($memberId) === __('you') || app(Identity::class)->memberId() === null;
    }

    public function refresh(): void
    {
        $this->issued = [];
        $this->load();
    }

    private function load(): void
    {
        $this->devices = [];

        try {
            $credentials = app(DirectorClient::class)->get('credentials')['credentials'] ?? [];
        } catch (DirectorRefused $refused) {
            $this->owner = ! $refused->is('owner_only');
            $this->problem = $this->owner ? $refused->explain() : '';

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();

            return;
        }

        $devices = array_values(array_filter(is_array($credentials) ? $credentials : [], fn ($credential): bool => is_array($credential) && ($credential['kind'] ?? null) === 'device'));
        usort($devices, fn (array $a, array $b): int => strcmp((string) ($b['created_at'] ?? ''), (string) ($a['created_at'] ?? '')));
        $this->devices = $devices;
    }
}; ?>

<div class="mx-auto flex w-full max-w-3xl flex-col gap-6">
    <div>
        <flux:heading size="xl" level="1">{{ __('Devices') }}</flux:heading>
        <flux:text class="mt-1">{{ __('Pair a phone, iPad or another computer with this Director. Each paired device has its own credential, and you can revoke it here.') }}</flux:text>
    </div>

    @if (! $owner)
        <flux:callout icon="information-circle" data-test="devices-not-owner">
            <flux:callout.text>{{ __('The Director does not let this credential manage devices. Use `cs device` on the Director\'s machine.') }}</flux:callout.text>
        </flux:callout>
    @else
        <flux:card class="flex flex-col gap-4">
            <flux:heading size="lg" level="2">{{ __('Pair a device') }}</flux:heading>

            @if ($issued !== [])
                @php($pairAddress = $issued['link'] !== '' ? \App\Client\AdvertisedUrl::address($issued['link']) : $address)
                <div class="flex flex-col gap-3" data-test="pairing-code">
                    @if ($issued['link'] !== '')
                        <div class="flex flex-wrap items-center gap-6">
                            <div class="rounded-lg bg-white p-3" data-test="pairing-qr" aria-label="{{ __('Pairing QR code') }}">
                                {!! \App\Client\QrCode::svg($issued['link']) !!}
                            </div>
                            <flux:text class="max-w-xs">{{ __('On :name, open ContextStack and tap Scan pairing code, or point the Camera at this code. Or enter the address and code below.', ['name' => $issued['name']]) }}</flux:text>
                        </div>
                    @else
                        <flux:text>{{ __('On :name, open ContextStack and enter:', ['name' => $issued['name']]) }}</flux:text>
                    @endif
                    <dl class="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-2">
                        <dt class="text-sm text-zinc-500">{{ __('Director address') }}</dt>
                        <dd class="font-mono text-lg" data-test="pairing-address">{{ $pairAddress !== '' ? $pairAddress : __('this machine\'s Tailscale address, port 8780') }}</dd>
                        <dt class="text-sm text-zinc-500">{{ __('Pairing code') }}</dt>
                        <dd class="font-mono text-2xl tracking-wider select-all" data-test="pairing-code-value">{{ $issued['code'] }}</dd>
                    </dl>
                    <flux:text class="text-sm">{{ __('The code works once, until') }} <x-client.time :at="$issued['expires_at']" />. {{ __('The device needs Tailscale connected.') }}</flux:text>
                    <div>
                        <flux:button size="sm" wire:click="refresh">{{ __('Done') }}</flux:button>
                    </div>
                </div>
            @else
                <form wire:submit="pair" class="flex items-end gap-3">
                    <flux:input wire:model="name" :label="__('Device name')" placeholder="ipad" class="flex-1" />
                    <flux:button type="submit" variant="primary">{{ __('Get a pairing code') }}</flux:button>
                </form>
            @endif
        </flux:card>

        @php($groups = ['mine' => array_values(array_filter($devices, fn (array $device): bool => $this->mine($device))), 'others' => array_values(array_filter($devices, fn (array $device): bool => ! $this->mine($device)))])
        @foreach ($groups as $group => $list)
            @continue($group === 'others' && $list === [])
            <div class="flex flex-col gap-2" data-test="devices-{{ $group }}">
                <flux:heading size="lg" level="2">{{ $group === 'mine' ? ($groups['others'] === [] ? __('Paired devices') : __('Your devices')) : __('Other members\' devices') }}</flux:heading>

                @forelse ($list as $device)
                    <div class="flex items-center justify-between gap-4 rounded-lg border border-zinc-200 px-4 py-3 dark:border-zinc-700" wire:key="device-{{ $device['id'] }}">
                        <div class="min-w-0">
                            <div class="font-medium">{{ $device['name'] ?? $device['id'] }}</div>
                            <div class="text-sm text-zinc-500">
                                @if ($group === 'others' && ($who = $this->owner($device)) !== null)
                                    <span data-test="device-member">{{ $who }}</span> ·
                                @endif
                                {{ __('Paired') }} <x-client.time :at="$device['created_at']" />
                                @if (! empty($device['last_used_at']))
                                    · {{ __('last used') }} <x-client.time :at="$device['last_used_at']" />
                                @endif
                            </div>
                        </div>
                        @if ($device['id'] === $self)
                            <flux:badge size="sm" data-test="this-device">{{ __('This device') }}</flux:badge>
                        @elseif (! empty($device['revoked_at']))
                            <flux:badge size="sm">{{ __('Revoked') }}</flux:badge>
                        @else
                            <flux:button size="sm" variant="danger" wire:click="revoke('{{ $device['id'] }}')"
                                wire:confirm="{{ __('Revoke :name? It will no longer reach the Director until it is paired again.', ['name' => $device['name'] ?? $device['id']]) }}">
                                {{ __('Revoke') }}
                            </flux:button>
                        @endif
                    </div>
                @empty
                    <flux:text>{{ __('No devices are paired yet.') }}</flux:text>
                @endforelse
            </div>
        @endforeach
    @endif

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif
</div>
