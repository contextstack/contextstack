<?php

namespace App\NativeComponents;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Json;
use App\Native\PairingLink;
use Illuminate\Support\Facades\Log;
use Illuminate\View\View;
use Native\Mobile\Attributes\On;
use Native\Mobile\Edge\NativeComponent;
use Native\Mobile\Events\Scanner\CodeScanned;
use Native\Mobile\Facades\Scanner;
use RuntimeException;

/**
 * Pair this device with a Director: its URL (over Tailscale, for now) and a one-time code from `cs-director pair NAME`
 * on the Director's machine (or Devices in the desktop app). The token it gets back is the only thing the app keeps.
 */
class Connect extends NativeComponent
{
    public string $url = '';

    public string $code = '';

    public string $problem = '';

    public function mount(): void
    {
        $this->url = app(ConnectionStore::class)->url();

        // Opened from a pairing link (the desktop's QR code scanned with the Camera app): fill both fields and let the
        // owner tap Pair, so a link from anywhere else cannot pair this device on its own.
        $link = PairingLink::parse('contextstack://pair?'.http_build_query(['url' => Json::string($this->query('url')), 'code' => Json::string($this->query('code'))]));
        if ($link !== null) {
            $this->url = $link['url'];
            $this->code = $link['code'];
        }
    }

    public function scan(): void
    {
        Scanner::scan()->formats(['qr'])->prompt('Scan the pairing code in the desktop app\'s Devices page')->scan();
    }

    /**
     * The in-app scanner read a code: a ContextStack pairing link pairs at once, since the owner chose to scan it.
     */
    #[On(CodeScanned::class)]
    public function scanned(string $data): void
    {
        $link = PairingLink::parse($data);
        if ($link === null) {
            $this->problem = 'That is not a ContextStack pairing code. Scan the one in the desktop app\'s Devices page.';

            return;
        }

        $this->url = $link['url'];
        $this->code = $link['code'];
        $this->pair($link['code']);
    }

    public function pair(?string $submitted = null): void
    {
        $code = trim($submitted ?? $this->code);
        $url = trim($this->url);

        if ($url === '' || preg_match('#^https?://#', $url) !== 1) {
            $this->problem = 'Enter the Director\'s full address, including http or https at the start.';

            return;
        }

        if ($code === '') {
            $this->problem = 'Enter the pairing code.';

            return;
        }

        try {
            $paired = DirectorClient::pair($url, $code, self::timeout());
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->is('pairing_refused')
                ? 'That code was not accepted. Codes work once and for 10 minutes; get a new one from Devices in the desktop app.'
                : $refused->explain();

            return;
        } catch (DirectorUnavailable $unavailable) {
            Log::warning('Pairing could not reach the Director.', ['error' => $unavailable->getMessage()]);
            $this->problem = 'The Director could not be reached at that address. Check that Tailscale is on and the address is right.';

            return;
        }

        try {
            app(ConnectionStore::class)->saveDevice($url, $paired['token'], $paired['credential_id'], $paired['name']);
        } catch (RuntimeException) {
            $this->problem = 'Paired, but the token could not be saved in the Keychain. Pair again with a new code.';

            return;
        }
        $this->code = '';
        $this->problem = '';
        $this->replace('/');
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
