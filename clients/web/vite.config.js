import tailwindcss from '@tailwindcss/vite';
import laravel from 'laravel-vite-plugin';
import { local } from 'laravel-vite-plugin/fonts';
import { defineConfig, lazyPlugins } from 'vite-plus';

export default defineConfig({
    plugins: lazyPlugins(() => [
        laravel({
            input: [
                'resources/css/app.css',
                'resources/js/client.js',
            ],
            refresh: true,
            fonts: [
                // Kept in the repo (OFL, see resources/fonts/instrument-sans/OFL.txt), so a release build needs no network.
                local('Instrument Sans', {
                    optimizedFallbacks: false,
                    variants: [
                        { src: 'resources/fonts/instrument-sans/instrument-sans-400-normal.woff2', weight: 400 },
                        { src: 'resources/fonts/instrument-sans/instrument-sans-500-normal.woff2', weight: 500 },
                        { src: 'resources/fonts/instrument-sans/instrument-sans-600-normal.woff2', weight: 600 },
                    ],
                }),
            ],
        }),
        tailwindcss(),
    ]),
    server: {
        cors: true,
        watch: {
            ignored: [
                '**/.agents/**',
                '**/.claude/**',
                '**/.cursor/**',
                '**/.junie/**',
                '**/storage/framework/views/**',
                '**/vendor/**',
            ],
        },
    },
});
