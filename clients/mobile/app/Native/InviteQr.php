<?php

namespace App\Native;

use Endroid\QrCode\Builder\Builder;
use Throwable;

/**
 * A QR code for a just-issued invite or pairing link, as a PNG data URI for `native:image`. It is drawn in memory for
 * the one answer that shows it and never written to disk. Without the image extension it needs, it is '' and the
 * screen shows the code and link alone.
 */
final class InviteQr
{
    public static function dataUri(string $link): string
    {
        if (! extension_loaded('gd')) {
            return '';
        }

        try {
            return (new Builder(data: $link, size: 320, margin: 12))->build()->getDataUri();
        } catch (Throwable) {
            return '';
        }
    }
}
