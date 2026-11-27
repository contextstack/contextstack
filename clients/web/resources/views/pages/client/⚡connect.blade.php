<?php

use App\Client\ChiefOfStaff;
use App\Client\Connection;
use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Identity;
use App\Client\InviteLink;
use App\Client\TailscaleAddress;
use Flux\Flux;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Locked;
use Livewire\Attributes\Title;
use Livewire\Component;

/*
 * client-web's settings: this device's connection to its Director (CLIENT-API.md §2). The Director URL; pairing
 * with a one-time code from `cs device pair NAME` (the device token is kept in this device's settings file, never
 * shown); on the Director's own machine, its local credential file; and this device's identity as the Director
 * sees it (`GET me`): the member it acts for and that member's role. An invited person joins here too, with the
 * invite code (or the whole `contextstack://invite` link) an owner or admin gave them: the Director answers with a
 * device token just as pairing does, kept the same way. There is no account and no login (outline decision 7).
 * An owner (manage_org_settings) sets the Director's advertised URL here: the address every invite and pairing link
 * names (`GET`/`PUT advertised-url`). Unset, this machine's Tailscale address is offered as a suggestion.
 */
new #[Layout('layouts::client')] #[Title('Settings')] class extends Component
{
    public string $url = '';

    public string $code = '';

    public string $problem = '';

    /** An invite code, or the whole contextstack://invite link. Cleared once used; never kept anywhere else. */
    public string $inviteCode = '';

    public string $inviteDevice = '';

    public string $inviteProblem = '';

    /** @var array<string, mixed> */
    public array $me = [];

    /**
     * The home machine for conversations that run tasks (`GET lead-node`): `{node, source}`, or [] when the Director
     * has no such setting (an older release answers 404) and the card stays hidden.
     *
     * @var array{node?: string, source?: string}
     */
    #[Locked]
    public array $home = [];

    /** @var list<string> the machines the owner can choose from */
    #[Locked]
    public array $machines = [];

    public string $homeChoice = '';

    public string $homeProblem = '';

    /** Whether the advertised-URL card shows: an owner on a Director that has the setting. */
    #[Locked]
    public bool $advertisedShown = false;

    /** The advertised URL the Director holds, '' when unset. */
    #[Locked]
    public string $advertised = '';

    /** This machine's address for other devices, offered when nothing is advertised ('' when unknown). */
    #[Locked]
    public string $advertisedSuggestion = '';

    public string $advertisedInput = '';

    public string $advertisedProblem = '';

    /** The chief-of-staff slot (ChiefOfStaff::slot, held by the Director): on or off, and its name. */
    public bool $chiefOfStaff = false;

    public string $chiefOfStaffName = '';

    public string $chiefOfStaffProblem = '';

    /**
     * On: the chief of staff's own conversation is started (or, if it was turned off before, unarchived). Off: it is
     * archived, to come back to. A rename while on only changes the name in the sidebar.
     */
    public function saveChiefOfStaff(ChiefOfStaff $chief): void
    {
        $this->chiefOfStaffProblem = '';
        $this->validate(['chiefOfStaffName' => ['required', 'string', 'max:'.ChiefOfStaff::NAME_LIMIT]]);
        $name = trim($this->chiefOfStaffName);

        try {
            $conversationId = $chief->save($this->chiefOfStaff, $name);
        } catch (DirectorRefused|DirectorUnavailable $failure) {
            $this->chiefOfStaffProblem = $failure instanceof DirectorRefused
                ? __('The Director refused: :why', ['why' => $failure->explain()])
                : $failure->getMessage();

            return;
        }

        if ($conversationId !== null) {
            Flux::toast(variant: 'success', text: __(':name is ready, pinned at the top of the sidebar.', ['name' => $name]));
            // A full load, not wire:navigate: the sidebar is kept across navigation and would keep the old slot.
            $this->redirect(route('conversations.show', $conversationId));

            return;
        }

        Flux::toast(variant: 'success', text: $this->chiefOfStaff
            ? __('Saved.')
            : __(':name is off. Turn it back on to pick up its conversation where you left off.', ['name' => $name]));
        $this->redirect(route('connect'));
    }

    public function saveHome(DirectorClient $director): void
    {
        $this->homeProblem = '';
        $this->validate(['homeChoice' => ['required', 'string', 'in:'.implode(',', $this->machines ?: [''])]]);

        try {
            $answer = $director->replace('lead-node', ['node' => $this->homeChoice]);
        } catch (DirectorRefused $refused) {
            $this->homeProblem = $refused->is('unknown_node') ? __('The Director does not know that machine.') : $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->homeProblem = $unavailable->getMessage();

            return;
        }

        $this->home = ['node' => (string) ($answer['node'] ?? $this->homeChoice), 'source' => (string) ($answer['source'] ?? 'set')];
        $this->homeChoice = $this->home['node'];
        Flux::toast(variant: 'success', text: __('New conversations that run tasks will live on :node.', ['node' => $this->home['node']]));
    }

    public function saveAdvertised(DirectorClient $director): void
    {
        $this->advertisedInput = trim($this->advertisedInput);
        $this->validate(['advertisedInput' => ['required', 'url:http,https', 'max:500']]);
        $this->writeAdvertised($director, $this->advertisedInput);
    }

    public function useSuggestedAddress(DirectorClient $director): void
    {
        if ($this->advertisedSuggestion === '') {
            return;
        }

        $this->advertisedInput = $this->advertisedSuggestion;
        $this->writeAdvertised($director, $this->advertisedSuggestion);
    }

    public function clearAdvertised(DirectorClient $director): void
    {
        $this->writeAdvertised($director, null);
    }

    private function writeAdvertised(DirectorClient $director, ?string $url): void
    {
        $this->advertisedProblem = '';
        if (! $this->advertisedShown) {
            return;
        }

        try {
            $answer = $director->replace('advertised-url', ['url' => $url]);
        } catch (DirectorRefused $refused) {
            $this->advertisedProblem = $refused->status === 422
                ? __('The Director did not accept that address (:why). Use an origin such as https://director.example.ts.net, with no path.', ['why' => $refused->detail])
                : $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->advertisedProblem = $unavailable->getMessage();

            return;
        }

        $this->advertised = is_string($answer['url'] ?? null) ? $answer['url'] : '';
        $this->advertisedInput = $this->advertised;
        $this->loadSuggestion();
        Flux::toast(variant: 'success', text: $this->advertised !== ''
            ? __('Invite and pairing links now name :url.', ['url' => $this->advertised])
            : __('Cleared. Invite and pairing links name the address the issuing device uses.'));
    }

    public function mount(ConnectionStore $connections, ChiefOfStaff $chief): void
    {
        ['enabled' => $this->chiefOfStaff, 'name' => $this->chiefOfStaffName] = $chief->slot();

        $this->url = $connections->url();
        $this->check();
    }

    public function pair(ConnectionStore $connections): void
    {
        $this->validate([
            'url' => ['required', 'url:http,https', 'max:500'],
            'code' => ['required', 'string', 'max:64'],
        ]);

        try {
            $paired = DirectorClient::pair($this->url, $this->code, (int) config('client.timeout'));
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->is('pairing_refused')
                ? __('That code was not accepted. Codes work once and for 10 minutes; issue a new one with `cs device pair NAME`.')
                : $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();

            return;
        }

        $connections->saveDevice($this->url, $paired['token'], $paired['credential_id'], $paired['name']);
        $this->code = '';
        $this->check();
    }

    /**
     * Join the Director as an invited member: `POST /api/v1/invites/accept`, then keep the device token like a pairing.
     */
    public function acceptInvite(ConnectionStore $connections): void
    {
        $this->inviteProblem = '';
        $link = InviteLink::parse($this->inviteCode);
        if ($link !== null) {
            $this->url = $link['url'];
            $this->inviteCode = $link['code'];
        }

        $this->validate([
            'url' => ['required', 'url:http,https', 'max:500'],
            'inviteCode' => ['required', 'string', 'max:128'],
            'inviteDevice' => ['required', 'string', 'max:100', 'regex:/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/'],
        ], [
            'inviteDevice.regex' => __('Use letters, numbers, dots, dashes or underscores, starting with a letter or number (for example office-mac).'),
        ]);

        try {
            $joined = DirectorClient::acceptInvite($this->url, $this->inviteCode, $this->inviteDevice, (int) config('client.timeout'));
        } catch (DirectorRefused $refused) {
            $this->inviteProblem = match (true) {
                $refused->is('invite_refused') => __('That invite was not accepted. It may be mistyped, already used, cancelled or past its 24 hours. Ask whoever invited you for a new one.'),
                $refused->status === 429 => __('Too many tries. Wait :seconds and try again.', ['seconds' => trans_choice(':count second|:count seconds', (int) ($refused->body['retry_after'] ?? 60))]),
                default => $refused->explain(),
            };

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->inviteProblem = $unavailable->getMessage();

            return;
        }

        $connections->saveDevice($this->url, $joined['token'], $joined['credential_id'], $joined['name']);
        $this->inviteCode = '';
        $this->inviteDevice = '';
        $this->check();
        Flux::toast(variant: 'success', text: __('Joined as :name.', ['name' => (string) ($joined['member']['display_name'] ?? $joined['name'])]));
    }

    public function useLocal(ConnectionStore $connections): void
    {
        $this->validate(['url' => ['required', 'url:http,https', 'max:500']]);
        $connections->saveLocal($this->url);
        $this->check();
    }

    public function forget(ConnectionStore $connections): void
    {
        app(Identity::class)->forget();
        $connections->forget();
        $this->url = $connections->url();
        $this->check();
    }

    public function connection(): ?Connection
    {
        try {
            return app(ConnectionStore::class)->resolve();
        } catch (NotConnected) {
            return null;
        }
    }

    /**
     * Whether `me` advertises a capability; a Director from before teams answers with `owner` only.
     */
    public function meCan(string $capability): bool
    {
        $capabilities = $this->me['capabilities'] ?? null;

        return is_array($capabilities) ? in_array($capability, $capabilities, true) : (bool) ($this->me['owner'] ?? false);
    }

    private function check(): void
    {
        $this->me = [];
        $this->problem = '';
        // A new connection, or this page asking again: who this device is gets asked again everywhere.
        app(Identity::class)->forget();

        if ($this->connection() === null) {
            return;
        }

        try {
            $this->me = app(DirectorClient::class)->get('me');
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();
        }

        $this->loadHome();
        $this->loadAdvertised();
    }

    private function loadAdvertised(): void
    {
        $this->advertisedShown = false;
        $this->advertised = '';
        $this->advertisedSuggestion = '';
        $this->advertisedProblem = '';
        if ($this->me === [] || ! is_array($this->me['capabilities'] ?? null) || ! $this->meCan('manage_org_settings')) {
            return;
        }

        try {
            $answer = app(DirectorClient::class)->find('advertised-url');
        } catch (DirectorRefused|DirectorUnavailable) {
            return;
        }
        if ($answer === null) {
            // A Director from before the setting.
            return;
        }

        $this->advertisedShown = true;
        $this->advertised = is_string($answer['url'] ?? null) ? $answer['url'] : '';
        $this->advertisedInput = $this->advertised;
        $this->loadSuggestion();
    }

    private function loadSuggestion(): void
    {
        $connection = $this->connection();
        $this->advertisedSuggestion = $this->advertised === '' && $connection !== null
            ? app(TailscaleAddress::class)->directorAddress($connection)
            : '';
    }

    private function loadHome(): void
    {
        $this->home = [];
        $this->machines = [];
        if ($this->me === []) {
            return;
        }

        try {
            $director = app(DirectorClient::class);
            $home = $director->find('lead-node');
            if (! is_string($home['node'] ?? null) || $home['node'] === '') {
                return;
            }
            $this->home = ['node' => $home['node'], 'source' => (string) ($home['source'] ?? '')];
            $this->homeChoice = $home['node'];
            if ($this->meCan('manage_org_settings')) {
                $nodes = (array) ($director->get('nodes')['nodes'] ?? []);
                $this->machines = array_values(array_map(fn (array $node): string => (string) $node['name'],
                    array_filter($nodes, fn ($node): bool => is_array($node) && is_string($node['name'] ?? null) && ($node['revoked_at'] ?? null) === null)));
            }
        } catch (DirectorRefused|DirectorUnavailable) {
            // The setting is extra: without it the page still shows the connection.
        }
    }
};
?>

<section class="mx-auto flex w-full max-w-3xl flex-col gap-5" data-test="connect-page">
    <div>
        <flux:heading size="lg" level="1" class="text-lg!">{{ __('Settings') }}</flux:heading>
        <flux:text class="mt-0.5">{{ __('This device reaches its Director through the Client API. No account and no login: pair it once with a code, or use the local credential on the Director\'s own machine.') }}</flux:text>
    </div>

    <flux:card class="space-y-3" data-test="appearance">
        <flux:heading size="lg">{{ __('Appearance') }}</flux:heading>
        <flux:text>{{ __('System follows this computer\'s light or dark setting.') }}</flux:text>
        <flux:radio.group x-data variant="segmented" x-model="$flux.appearance" class="max-w-sm">
            <flux:radio value="system" icon="computer-desktop">{{ __('System') }}</flux:radio>
            <flux:radio value="light" icon="sun">{{ __('Light') }}</flux:radio>
            <flux:radio value="dark" icon="moon">{{ __('Dark') }}</flux:radio>
        </flux:radio.group>
    </flux:card>

    @php($connection = $this->connection())

    <flux:card class="space-y-2" data-test="current-connection">
        <flux:heading size="lg">{{ __('Now') }}</flux:heading>
        @if ($connection)
            <flux:text>{{ __('Director') }}: <span class="font-mono">{{ $connection->url }}</span></flux:text>
            <flux:text class="break-words [overflow-wrap:anywhere]">
                {{ __('Credential') }}:
                @if ($connection->mode === \App\Client\Connection::Device)
                    {{ __('this device, paired as :name (:id)', ['name' => $connection->name, 'id' => $connection->credentialId]) }}
                @else
                    {{ __('the local credential (:path)', ['path' => app(\App\Client\ConnectionStore::class)->localCredentialPath()]) }}
                @endif
            </flux:text>
            @if ($me !== [])
                @if (is_string($me['member']['display_name'] ?? null) && is_string($me['role'] ?? null))
                    <flux:text data-test="device-member">
                        {{ __('Signed in as') }} <span class="font-medium">{{ $me['member']['display_name'] }}</span>
                        · <span data-test="device-role">{{ __(ucfirst($me['role'])) }}</span>
                    </flux:text>
                @endif
                <flux:text data-test="device-identity">
                    {{ __('This device') }}: <span class="font-mono">{{ $me['credential_id'] ?? '?' }}</span>
                    @if (! is_array($me['capabilities'] ?? null))
                        {{ ($me['owner'] ?? false) ? __('(owner: may pair and revoke devices)') : __('(not the owner)') }}
                    @endif
                </flux:text>
                <flux:badge color="green" data-test="connected">{{ is_string($me['role'] ?? null) ? __('Connected as :role', ['role' => __($me['role'])]) : (($me['owner'] ?? false) ? __('Connected as owner') : __('Connected')) }}</flux:badge>
            @endif
            <div>
                <flux:button size="sm" variant="ghost" wire:click="forget">{{ $connection->mode === \App\Client\Connection::Device ? __('Forget this device') : __('Forget this connection') }}</flux:button>
            </div>
        @else
            <flux:text data-test="not-connected">{{ __('Not connected. Pair this device below.') }}</flux:text>
        @endif
    </flux:card>

    @if ($home !== [])
        <flux:card class="space-y-3" data-test="home-machine">
            <div>
                <flux:heading size="lg">{{ __('Home machine') }}</flux:heading>
                <flux:text class="mt-1">{{ __('Conversations that run tasks live on this machine. The task lanes they start can run on any machine; conversations already started stay where they are.') }}</flux:text>
            </div>
            @if ($machines !== [])
                <form wire:submit="saveHome" class="flex flex-wrap items-end gap-2">
                    <flux:select wire:model="homeChoice" class="max-w-xs" :aria-label="__('Home machine')" data-test="home-machine-select">
                        @foreach ($machines as $name)
                            <flux:select.option :value="$name" wire:key="home-{{ $name }}">{{ $name }}{{ $name === ($home['node'] ?? '') && ($home['source'] ?? '') === 'default' ? ' — '.__('the Director\'s machine') : '' }}</flux:select.option>
                        @endforeach
                    </flux:select>
                    <flux:button type="submit" size="sm">{{ __('Save') }}</flux:button>
                </form>
            @else
                <flux:text><span class="font-mono">{{ $home['node'] }}</span>@if (($home['source'] ?? '') === 'default') {{ __('(the Director\'s machine)') }}@endif</flux:text>
            @endif
            @if ($homeProblem !== '')
                <flux:text class="text-red-600 dark:text-red-400" data-test="home-machine-problem">{{ $homeProblem }}</flux:text>
            @endif
        </flux:card>
    @endif

    @if (! is_array($me['capabilities'] ?? null) || $this->meCan('manage_org_settings'))
    <flux:card class="space-y-3" data-test="chief-of-staff">
        <div>
            <flux:heading size="lg">{{ __('Chief of staff') }}</flux:heading>
            <flux:text class="mt-1">{{ __('One standing conversation that runs tasks for you, pinned at the top of the sidebar under a name you choose, and where the app opens. Turning it on starts that conversation; turning it off archives it until you turn it on again.') }}</flux:text>
        </div>
        <form wire:submit="saveChiefOfStaff" class="space-y-3">
            <flux:switch wire:model="chiefOfStaff" :label="__('On')" align="left" data-test="chief-of-staff-switch" />
            <div class="flex flex-wrap items-end gap-2">
                <flux:input wire:model="chiefOfStaffName" :label="__('Name')" maxlength="{{ \App\Client\ChiefOfStaff::NAME_LIMIT }}" class="max-w-xs" data-test="chief-of-staff-name" />
                <flux:button type="submit" size="sm" data-test="chief-of-staff-save">{{ __('Save') }}</flux:button>
            </div>
            @if ($chiefOfStaffProblem !== '')
                <flux:text class="text-red-600 dark:text-red-400" data-test="chief-of-staff-problem">{{ $chiefOfStaffProblem }}</flux:text>
            @endif
        </form>
    </flux:card>
    @endif

    @if ($advertisedShown)
        <flux:card class="space-y-3" data-test="advertised-url">
            <div>
                <flux:heading size="lg">{{ __('Director address for other devices') }}</flux:heading>
                <flux:text class="mt-1">{{ __('The address every invite and pairing link names, such as https://director.example.ts.net. Unset, each link names the address of the device that issued it.') }}</flux:text>
            </div>
            <flux:text data-test="advertised-url-current">
                {{ __('Now') }}:
                @if ($advertised !== '')
                    <span class="font-mono">{{ $advertised }}</span>
                @else
                    {{ __('not set') }}
                @endif
            </flux:text>
            <form wire:submit="saveAdvertised" class="flex flex-wrap items-end gap-2">
                <flux:input wire:model="advertisedInput" :label="__('Address')" placeholder="https://director.example.ts.net" class="max-w-sm" data-test="advertised-url-input" />
                <flux:button type="submit" size="sm" data-test="advertised-url-save">{{ __('Save') }}</flux:button>
                @if ($advertised !== '')
                    <flux:button size="sm" variant="ghost" wire:click="clearAdvertised" data-test="advertised-url-clear">{{ __('Clear') }}</flux:button>
                @endif
            </form>
            @if ($advertised === '' && $advertisedSuggestion !== '')
                <div class="flex flex-wrap items-center gap-2" data-test="advertised-url-suggestion">
                    <flux:text>{{ __('This machine is reachable at') }} <span class="font-mono">{{ $advertisedSuggestion }}</span></flux:text>
                    <flux:button size="sm" wire:click="useSuggestedAddress" data-test="advertised-url-use-suggestion">{{ __('Use this address') }}</flux:button>
                </div>
            @endif
            @if ($advertisedProblem !== '')
                <flux:text class="text-red-600 dark:text-red-400" data-test="advertised-url-problem">{{ $advertisedProblem }}</flux:text>
            @endif
        </flux:card>
    @endif

    @if ($me !== [] && $this->meCan('manage_org_settings') && is_array($me['capabilities'] ?? null))
        <flux:card class="flex items-center justify-between gap-3" data-test="peer-messages-link">
            <div>
                <flux:heading size="lg">{{ __('Messages between conversations') }}</flux:heading>
                <flux:text class="mt-1">{{ __('Choose which conversations may send each other briefs. Off until you allow a direction.') }}</flux:text>
            </div>
            <flux:button size="sm" :href="route('peer-messages')" wire:navigate>{{ __('Choose') }}</flux:button>
        </flux:card>
    @endif

    @if ($problem !== '')
        <flux:callout variant="danger" icon="exclamation-triangle" data-test="problem">
            <flux:callout.text>{{ $problem }}</flux:callout.text>
        </flux:callout>
    @endif

    <flux:card class="space-y-4">
        <flux:heading size="lg">{{ __('Pair this device') }}</flux:heading>
        <flux:text>{{ __('On the Director host, run `cs device pair NAME` and enter the code here within 10 minutes.') }}</flux:text>
        <form wire:submit="pair" class="space-y-4">
            <flux:input wire:model="url" :label="__('Director URL')" placeholder="http://127.0.0.1:8780" />
            <flux:input wire:model="code" :label="__('Pairing code')" autocomplete="off" />
            <flux:button type="submit" variant="primary">{{ __('Pair') }}</flux:button>
        </form>
    </flux:card>

    <flux:card class="space-y-4" data-test="accept-invite">
        <flux:heading size="lg">{{ __('Join with an invite') }}</flux:heading>
        <flux:text>{{ __('Someone who runs this Director invited you. Paste the invite link, or enter the Director URL above and the invite code, and name this device.') }}</flux:text>
        <form wire:submit="acceptInvite" class="space-y-4">
            <flux:input wire:model="inviteCode" :label="__('Invite code or link')" autocomplete="off" data-test="invite-code" />
            <flux:input wire:model="inviteDevice" :label="__('Name for this device')" placeholder="office-mac" />
            <flux:button type="submit" variant="primary">{{ __('Join') }}</flux:button>
        </form>
        @if ($inviteProblem !== '')
            <flux:text class="text-red-600 dark:text-red-400" data-test="invite-problem">{{ $inviteProblem }}</flux:text>
        @endif
    </flux:card>

    <flux:card class="space-y-4">
        <flux:heading size="lg">{{ __('Same machine as the Director') }}</flux:heading>
        <flux:text class="[overflow-wrap:anywhere]">{{ __('Use the Director\'s local credential, read from :path at each request.', ['path' => app(\App\Client\ConnectionStore::class)->localCredentialPath()]) }}</flux:text>
        <flux:button wire:click="useLocal">{{ __('Use the local credential') }}</flux:button>
    </flux:card>
</section>
