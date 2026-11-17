<!DOCTYPE html>
<html
    lang="{{ str_replace('_', '-', app()->getLocale()) }}"
    class="dark"
    data-native-desktop
    data-native-open-external-url="{{ route('native.open-external') }}"
>
    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1.0" />
        <meta name="csrf-token" content="{{ csrf_token() }}">
        <title>{{ filled($title ?? null) ? $title.' - ContextStack' : 'ContextStack' }}</title>
        <link rel="icon" href="/favicon.svg" type="image/svg+xml">
        <link rel="alternate icon" href="/favicon.ico" sizes="16x16 32x32">
        <link rel="icon" href="/favicon-32x32.png" type="image/png" sizes="32x32">
        <link rel="apple-touch-icon" href="/apple-touch-icon.png">
        @fonts
        @vite(['resources/css/app.css', 'resources/js/client.js'])
        @fluxAppearance
    </head>
    <body class="flex min-h-screen items-center justify-center bg-white px-6 dark:bg-zinc-800">
        <main class="w-full max-w-lg">
            {{ $slot }}
        </main>
        @fluxScripts
    </body>
</html>
