<?php

declare(strict_types=1);

namespace App\Http\Controllers;

use App\Client\NativeDesktop;
use Illuminate\Http\Request;
use Illuminate\Http\Response;
use Native\Desktop\Facades\Shell;

final class NativeShellController
{
    public function openExternal(Request $request): Response
    {
        abort_unless(NativeDesktop::isRunning(), Response::HTTP_NOT_FOUND);

        $validated = $request->validate([
            'url' => ['required', 'string', 'max:2048'],
        ]);
        $url = $validated['url'];

        // Only what the Markdown renderer links: the OS opens anything else (file:, custom app schemes) as an action.
        $scheme = strtolower((string) parse_url($url, PHP_URL_SCHEME));
        $openable = match ($scheme) {
            'http', 'https' => filter_var($url, FILTER_VALIDATE_URL) !== false,
            'mailto' => preg_match('/\Amailto:[^\s<>"]+\z/', $url) === 1,
            default => false,
        };
        abort_unless($openable, Response::HTTP_UNPROCESSABLE_ENTITY);

        Shell::openExternal($url);

        return response()->noContent();
    }
}
