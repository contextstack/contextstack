<?php

declare(strict_types=1);

namespace App\Client;

use Illuminate\Support\Facades\Cache;
use Phiki\Phiki;
use Throwable;

/**
 * Syntax colouring for a code block in a message, with Phiki (TextMate grammars, as in VS Code). Each token carries
 * its light colour and its dark one as `--phiki-dark-color`; app.css switches to the dark one in dark mode.
 */
final class CodeColour
{
    private const LIGHT = 'github-light';

    private const DARK = 'github-dark';

    /** How long a coloured block is kept: a thread re-renders its turns often, and a finished turn never changes. */
    private const KEEP_SECONDS = 86400;

    private static ?Phiki $phiki = null;

    /**
     * The coloured tokens to put inside the block's <code>, or null to show it plain: no language, one Phiki does not
     * know, a block over MarkdownHtml::COLOUR_LIMIT, or any failure inside Phiki.
     */
    public static function html(string $code, string $language): ?string
    {
        $language = strtolower($language);
        if ($language === '' || strlen($code) > MarkdownHtml::COLOUR_LIMIT) {
            return null;
        }
        $phiki = self::$phiki ??= new Phiki;
        if (! $phiki->environment->grammars->has($language)) {
            return null;
        }

        return Cache::remember('code-colour:'.md5($language."\0".$code), self::KEEP_SECONDS, static function () use ($phiki, $code, $language): ?string {
            try {
                $html = $phiki->codeToHtml(rtrim($code, "\n"), $language, ['light' => self::LIGHT, 'dark' => self::DARK])->toString();
            } catch (Throwable) {
                return null;
            }

            return preg_match('/<code[^>]*>(.*)<\/code>/s', $html, $inner) === 1 ? $inner[1] : null;
        });
    }
}
