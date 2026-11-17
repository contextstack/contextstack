@props(['text', 'breaks' => false, 'colour' => true])

{{-- Markdown as safe HTML (App\Client\MarkdownHtml). Code blocks are framed by .cs-code in app.css. --}}
<div {{ $attributes->class('min-w-0 break-words text-sm leading-relaxed [&_p+p]:mt-3 [&_ul]:my-3 [&_ul]:list-disc [&_ul]:pl-6 [&_ol]:my-3 [&_ol]:list-decimal [&_ol]:pl-6 [&_a]:underline [&_a]:underline-offset-2 [&_:not(pre)>code]:rounded-md [&_:not(pre)>code]:bg-black/[.06] [&_:not(pre)>code]:px-1.5 [&_:not(pre)>code]:py-0.5 [&_:not(pre)>code]:font-mono [&_:not(pre)>code]:text-[.875em] dark:[&_:not(pre)>code]:bg-white/10 [&_h1]:text-xl [&_h2]:text-lg [&_h3]:text-base [&_h1]:font-semibold [&_h2]:font-semibold [&_h3]:font-semibold [&_blockquote]:border-l-2 [&_blockquote]:border-zinc-400 [&_blockquote]:pl-4') }}>
    {!! \App\Client\MarkdownHtml::render($text, $breaks, $colour) !!}
</div>
