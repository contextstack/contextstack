<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\IdentityStore;
use App\Native\PairingLink;
use Illuminate\Support\Facades\Log;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Edge\NativeComponent;
use Native\Mobile\Events\Scanner\CodeScanned;
use Native\Mobile\Facades\Scanner;
use RuntimeException;

/**
 * Connect this device to a Director, one of two ways. A pairing code (`cs-director pair NAME`, or Devices in
 * any connected app) adds another device for the same member. An invite code (Members in any connected app) joins the
 * Director as the member it was issued for. Both come as a code to type, a contextstack://pair or contextstack://invite
 * link to open or paste, or a QR code to scan. An invite always asks the person to confirm the Director's address
 * first, even from the scanner, since joining hands that Director a new device credential.
 *
 * The token that comes back goes to the Keychain; the code is never logged or kept once it is used.
 */
class Connect extends NativeComponent
{
    public const string PairChoice = 'Pairing code';

    public const string InviteChoice = 'Invite code';

    /** The navigation data key the locked screen hands a link's query over in. */
    public const string ReturnQuery = 'returnQuery';

    public string $url = '';

    public string $code = '';

    /** Which code the person has: PairChoice or InviteChoice. */
    public string $have = self::PairChoice;

    /** The name this device gets on the Director when it joins by invite. */
    public string $deviceName = 'iPad';

    /** An invite waits for the person to confirm the Director's address. */
    public bool $confirming = false;

    public string $problem = '';

    /** The Director this device is already paired with, or '' when it is not paired. */
    public string $pairedWith = '';

    /**
     * Which link this route opens: /pair or /invite.
     *
     * @var 'pair'|'invite'
     */
    protected string $linkKind = PairingLink::Pair;

    public function mount(): void
    {
        $store = app(ConnectionStore::class);
        $this->url = $store->url();
        $this->pairedWith = $store->isConnected() ? $this->url : '';

        // Opened from a link (a QR code scanned with the Camera app, or a link tapped elsewhere): fill the fields and
        // wait for a tap, so a link from anywhere else cannot connect this device on its own.
        $handedOver = $this->data(self::ReturnQuery);
        $query = is_array($handedOver) ? $handedOver : $this->query();
        $query = is_array($query) ? $query : [];
        $link = PairingLink::fields($this->linkKind, $query['url'] ?? null, $query['code'] ?? null);
        if ($link !== null) {
            $this->fillFrom($link);
        }
        // The code now lives only in this screen's field; drop it from the route's query.
        $this->setQuery([]);
    }

    public function inviting(): bool
    {
        return $this->have === self::InviteChoice;
    }

    /**
     * @return list<string>
     */
    public function haveChoices(): array
    {
        return [self::PairChoice, self::InviteChoice];
    }

    public function scan(): void
    {
        Scanner::scan()->formats(['qr'])->prompt('Scan a ContextStack pairing or invite code')->scan();
    }

    /**
     * The in-app scanner read a code. A pairing link pairs at once, since the person chose to scan it; an invite link
     * fills the fields and asks them to confirm the Director's address.
     */
    #[On(CodeScanned::class)]
    public function scanned(string $data): void
    {
        $link = PairingLink::parse($data);
        if ($link === null) {
            $this->problem = 'That is not a ContextStack pairing or invite code. Scan the one another ContextStack app shows under Devices or Members.';

            return;
        }

        $this->fillFrom($link);
        if ($link['kind'] === PairingLink::Pair) {
            $this->pair($link['code']);
        }
    }

    /**
     * Pair, or join by invite (the button's label follows the kind of code). A whole link pasted into the code field
     * is read as that link.
     */
    public function pair(?string $submitted = null): void
    {
        $code = trim($submitted ?? $this->code);
        $link = PairingLink::parse($code);
        if ($link !== null) {
            $this->fillFrom($link);
            if ($link['kind'] === PairingLink::Invite) {
                return;
            }
            $code = $link['code'];
        }

        $url = trim($this->url);
        if ($url === '' || preg_match('#^https?://#', $url) !== 1) {
            $this->problem = 'Enter the Director\'s full address, including http or https at the start.';

            return;
        }

        if ($code === '') {
            $this->problem = $this->inviting() ? 'Enter the invite code.' : 'Enter the pairing code.';

            return;
        }

        if ($this->inviting()) {
            $this->confirming = true;
            $this->problem = '';

            return;
        }

        try {
            $paired = DirectorClient::pair($url, $code, self::timeout());
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->is('pairing_refused')
                ? 'That code was not accepted. Codes work once and for 10 minutes; get a new one from Devices in any ContextStack app that is already connected.'
                : $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->unreachable($unavailable);

            return;
        }

        $this->keep($url, $paired);
    }

    /**
     * The person confirmed the Director's address: exchange the invite code.
     */
    public function confirmJoin(): void
    {
        $url = trim($this->url);
        $code = trim($this->code);
        $name = trim($this->deviceName);
        if (! $this->inviting() || $code === '' || preg_match('#^https?://#', $url) !== 1) {
            $this->confirming = false;

            return;
        }
        if (preg_match('/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/', $name) !== 1) {
            $this->problem = 'Name this device with letters, numbers, dots, dashes or underscores, starting with a letter or number (for example iPad).';

            return;
        }

        try {
            $joined = DirectorClient::acceptInvite($url, $code, $name, self::timeout());
        } catch (DirectorRefused $refused) {
            $this->confirming = false;
            if ($refused->is('invite_refused')) {
                $this->code = '';
            }
            $this->problem = $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            $this->unreachable($unavailable);

            return;
        }

        $this->keep($url, $joined);
    }

    public function cancelJoin(): void
    {
        $this->confirming = false;
        $this->code = '';
        $this->problem = '';
    }

    /**
     * @param  array{kind: 'pair'|'invite', url: string, code: string}  $link
     */
    private function fillFrom(array $link): void
    {
        $this->url = $link['url'];
        $this->code = $link['code'];
        $this->have = $link['kind'] === PairingLink::Invite ? self::InviteChoice : self::PairChoice;
        $this->confirming = $link['kind'] === PairingLink::Invite;
        $this->problem = '';
    }

    /**
     * Save the new token through the Keychain path, then read who it acts for.
     *
     * @param  array{credential_id: string, name: string, token: string}  $answer
     */
    private function keep(string $url, array $answer): void
    {
        try {
            app(ConnectionStore::class)->saveDevice($url, $answer['token'], $answer['credential_id'], $answer['name']);
        } catch (RuntimeException) {
            $this->problem = 'Connected, but the token could not be saved in the Keychain. Try again with a new code.';

            return;
        }
        $identity = app(IdentityStore::class);
        $identity->forget();
        $identity->current();

        $this->code = '';
        $this->confirming = false;
        $this->problem = '';
        $this->replace('/');
    }

    private function unreachable(DirectorUnavailable $unavailable): void
    {
        Log::warning('Connecting could not reach the Director.', ['error' => $unavailable->getMessage()]);
        $this->problem = 'The Director could not be reached at that address. Check that Tailscale is on and the address is right.';
    }

    private static function timeout(): int
    {
        $timeout = config('client.timeout');

        return is_int($timeout) ? $timeout : 15;
    }

    public function render(): View
    {
        return view('native.connect');
    }
}
