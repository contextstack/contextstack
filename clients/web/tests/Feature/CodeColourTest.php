<?php

use App\Client\MarkdownHtml;

it('colours a code block in a language it knows, with a dark colour for each token', function () {
    $html = MarkdownHtml::render("```rust\nfn main() { let x = \"hi\"; }\n```");

    expect($html)->toContain('<pre class="cs-coloured"><code class="language-rust">')
        ->toContain('<span class="token" style="color: ')
        ->toContain('--phiki-dark-color:')
        ->toContain('>fn</span>')
        ->toContain('</code></pre></div>');
});

it('keeps code text escaped once coloured', function () {
    $html = MarkdownHtml::render("```php\n<?php echo '<script>alert(1)</script>'; ?>\n```");

    expect($html)->toContain('cs-coloured')
        ->not->toContain('<script>')
        ->toContain('&lt;')
        ->toContain('script');
});

it('shows a block plain when it has no language, one it does not know, or is too long', function (string $fence, string $code, string $opening) {
    $html = MarkdownHtml::render($fence."\n".$code."\n```");

    expect($html)->not->toContain('cs-coloured')
        ->toContain($opening)
        ->toContain('data-code-block');
})->with([
    'no language' => ['```', 'echo 1;', '<pre><code>'],
    'unknown language' => ['```notalanguage', 'echo 1;', '<pre><code class="language-notalanguage">'],
    'too long' => ['```php', str_repeat('$a = 1;'."\n", 2000), '<pre><code class="language-php">'],
]);

it('keeps the language bar and copy button on a coloured block', function () {
    expect(MarkdownHtml::render("```python\nprint('hi')\n```"))
        ->toContain('<span>python</span>')
        ->toContain('data-test="copy-code"');
});

it('leaves code plain when asked to, as for a reply still streaming in', function () {
    expect(MarkdownHtml::render("```php\necho 1;\n```", colour: false))
        ->not->toContain('cs-coloured')
        ->toContain('<pre><code class="language-php">echo 1;');
});
