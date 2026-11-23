<?php

namespace App\Client;

use BaconQrCode\Renderer\Image\SvgImageBackEnd;
use BaconQrCode\Renderer\ImageRenderer;
use BaconQrCode\Renderer\RendererStyle\RendererStyle;
use BaconQrCode\Writer;

/**
 * A QR code as inline SVG, made on this machine (nothing is sent anywhere to draw it).
 */
class QrCode
{
    public static function svg(string $text, int $size = 220): string
    {
        $svg = (new Writer(new ImageRenderer(new RendererStyle($size, 1), new SvgImageBackEnd)))->writeString($text);

        return trim((string) preg_replace('/^<\?xml[^>]*>\s*/', '', $svg));
    }

    /**
     * The link the mobile app's scanner and the iPad's Camera app understand: contextstack://pair?url=URL&code=CODE.
     */
    public static function pairingLink(string $url, string $code): string
    {
        return 'contextstack://pair?'.http_build_query(['url' => $url, 'code' => $code]);
    }
}
