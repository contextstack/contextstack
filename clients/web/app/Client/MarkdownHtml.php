<?php

declare(strict_types=1);

namespace App\Client;

use Illuminate\Support\Str;

final class MarkdownHtml
{
    /**
     * Safe HTML for Markdown: raw HTML escaped, no images, unsafe links dropped. Code blocks get a bar with their
     * language and a copy button. With $breaks, a single line break stays a line break, as typed (a person's message);
     * otherwise it is Markdown's soft break (a model's reply). Without $colour, code blocks stay plain: a reply still
     * streaming in changes on every update, and colouring it each time would slow the page.
     */
    public static function render(string $text, bool $breaks = false, bool $colour = true): string
    {
        $options = ['html_input' => 'escape', 'allow_unsafe_links' => false];
        if ($breaks) {
            $options['renderer'] = ['soft_break' => "<br />\n"];
        }
        $html = self::withCodeBars(Str::markdown($text, $options, [new MarkdownWithoutImages]), $colour);

        if (! NativeDesktop::isRunning()) {
            return $html;
        }

        return self::markExternalLinks($html);
    }

    /** Longest code block that is coloured; a longer one shows plain, so a huge paste never stalls a page. */
    public const COLOUR_LIMIT = 12000;

    /**
     * Wraps each code block in a frame whose bar names its language and copies the code. The copy runs in the page
     * (Alpine), so it needs nothing from the server and works while a reply is still streaming in. A block whose
     * language Phiki knows is coloured (light and dark, see .cs-code in app.css).
     */
    private static function withCodeBars(string $html, bool $colour): string
    {
        if (! str_contains($html, '<pre>')) {
            return $html;
        }

        $copy = e(__('Copy'));
        $copied = e(__('Copied'));
        $icon = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="currentColor" class="size-3.5" aria-hidden="true"><path d="M5.5 3.5A1.5 1.5 0 0 1 7 2h2.879a1.5 1.5 0 0 1 1.06.44l2.122 2.12a1.5 1.5 0 0 1 .439 1.061V9.5A1.5 1.5 0 0 1 12 11V8.621a3 3 0 0 0-.879-2.121L9 4.379A3 3 0 0 0 6.879 3.5H5.5Z"/><path d="M4 5a1.5 1.5 0 0 0-1.5 1.5v6A1.5 1.5 0 0 0 4 14h5a1.5 1.5 0 0 0 1.5-1.5V8.621a1.5 1.5 0 0 0-.44-1.06L7.94 5.439A1.5 1.5 0 0 0 6.878 5H4Z"/></svg>';

        return (string) preg_replace_callback(
            '/<pre><code(?: class="language-([^"]*)")?>(.*?)<\/code><\/pre>/s',
            static function (array $matches) use ($copy, $copied, $icon, $colour): string {
                $language = $matches[1];
                $label = $language !== '' ? e($language) : e(__('Code'));
                $code = ! $colour ? null : CodeColour::html(html_entity_decode($matches[2], ENT_QUOTES | ENT_HTML5), $language);
                $open = $code === null ? '<pre><code'.($language !== '' ? ' class="language-'.$language.'"' : '').'>' : '<pre class="cs-coloured"><code class="language-'.$language.'">';

                return '<div class="cs-code" data-code-block><div class="cs-code-bar"><span>'.$label.'</span>'
                    .'<button type="button" class="cs-code-copy" x-data="{ copied: false }" '
                    .'x-on:click="if (await csCopy($el.closest(\'[data-code-block]\').querySelector(\'code\').innerText)) { copied = true; setTimeout(() => copied = false, 1500) }" '
                    .'data-test="copy-code">'.$icon.'<span x-text="copied ? \''.$copied.'\' : \''.$copy.'\'">'.$copy.'</span></button></div>'
                    .$open.($code ?? $matches[2]).'</code></pre></div>';
            },
            $html,
        );
    }

    public static function isSameOrigin(string $href, ?string $baseUrl = null): bool
    {
        if ($href === '' || str_starts_with($href, '#')) {
            return true;
        }

        if (! parse_url($href, PHP_URL_SCHEME)) {
            return true;
        }

        $baseUrl ??= url('/');
        $base = parse_url($baseUrl);
        $target = parse_url($href);

        return ($base['scheme'] ?? null) === ($target['scheme'] ?? null)
            && ($base['host'] ?? null) === ($target['host'] ?? null)
            && ($base['port'] ?? null) === ($target['port'] ?? null);
    }

    private static function markExternalLinks(string $html): string
    {
        if (! str_contains($html, '<a ')) {
            return $html;
        }

        return (string) preg_replace_callback(
            '/<a\s+href=(["\'])(.*?)\1([^>]*)>/i',
            static function (array $matches): string {
                $quote = $matches[1];
                $href = html_entity_decode($matches[2], ENT_QUOTES | ENT_HTML5);

                if (self::isSameOrigin($href)) {
                    return $matches[0];
                }

                if (str_contains($matches[3], 'data-native-external-link')) {
                    return $matches[0];
                }

                // $matches[2] is the href as CommonMark wrote it, already escaped for the attribute.
                return '<a href='.$quote.$matches[2].$quote
                    .' data-native-external-link'.$matches[3].'>';
            },
            $html,
        );
    }
}
