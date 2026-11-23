<!DOCTYPE html>
<html
    lang="{{ str_replace('_', '-', app()->getLocale()) }}"
    @if (\App\Client\NativeDesktop::isRunning())
        data-native-desktop
        data-native-open-external-url="{{ route('native.open-external') }}"
    @endif
>
    <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1.0" />
        @if (\App\Client\NativeDesktop::isRunning())
            <meta name="csrf-token" content="{{ csrf_token() }}">
        @endif
        <title>{{ filled($title ?? null) ? $title.' - ContextStack' : 'ContextStack' }}</title>
        <link rel="icon" href="/favicon.svg" type="image/svg+xml">
        <link rel="alternate icon" href="/favicon.ico" sizes="16x16 32x32">
        <link rel="icon" href="/favicon-32x32.png" type="image/png" sizes="32x32">
        <link rel="apple-touch-icon" href="/apple-touch-icon.png">
        @fonts
        @vite(['resources/css/app.css', 'resources/js/client.js'])
        @fluxAppearance
    </head>
    {{--
        The app frame, like a chat app's: a thin icon rail, a collapsible thread panel, and one main column. The
        frame never scrolls; the main column does, or a page that marks itself data-full-bleed scrolls its own panes.
    --}}
    <body class="h-dvh overflow-hidden bg-white text-zinc-900 antialiased dark:bg-cs-main dark:text-zinc-100">
        <div class="flex h-full">
            @persist('sidebar')
                <div x-data="{
                        panel: true,
                        drawer: false,
                        narrow: ! window.matchMedia('(min-width: 768px)').matches,
                        init() {
                            try { this.panel = localStorage.getItem('cs.sidebar') !== 'closed' } catch (e) {}
                            window.matchMedia('(min-width: 768px)').addEventListener('change', (event) => { this.narrow = ! event.matches });
                            // Opening the drawer moves focus into it, so keyboard users land in what they opened.
                            this.$watch('drawer', (open) => open && this.$nextTick(() => this.$refs.drawer?.querySelector('a, button')?.focus()));
                        },
                        togglePanel() { this.panel = ! this.panel; try { localStorage.setItem('cs.sidebar', this.panel ? 'open' : 'closed') } catch (e) {} },
                     }"
                     x-on:cs-sidebar.window="window.matchMedia('(min-width: 768px)').matches ? togglePanel() : (drawer = true)"
                     x-on:livewire:navigated.window="drawer = false"
                     x-on:keydown.escape.window="drawer = false"
                     class="contents">
                    <div x-show="drawer" x-cloak x-transition.opacity x-on:click="drawer = false" class="fixed inset-0 z-30 bg-black/40 md:hidden"></div>

                    <div class="fixed inset-y-0 left-0 z-40 flex transition-transform duration-200 max-md:-translate-x-full md:static md:h-full"
                         x-ref="drawer" x-bind:class="drawer && 'max-md:!translate-x-0'" x-bind:inert="narrow && ! drawer">
                        <nav class="flex w-[3.25rem] shrink-0 flex-col items-center gap-1 bg-cs-shell-light py-2.5 dark:bg-cs-shell" data-test="nav-rail">
                            <a href="{{ route('home') }}" class="mb-2 flex size-9 items-center justify-center" aria-label="ContextStack">
                                {{-- The website's mark (~/sites/contextstack public/images/contextstack-mark.svg). --}}
                                <img src="/images/contextstack-mark.svg" alt="" class="size-9 rounded-lg" data-test="brand-mark">
                            </a>
                            <flux:tooltip :content="__('Show sidebar')" position="right">
                                <button type="button" x-show="! panel && ! drawer" x-cloak x-on:click="togglePanel()" aria-label="{{ __('Show sidebar') }}" data-test="sidebar-show"
                                        class="flex size-9 items-center justify-center rounded-lg text-zinc-500 hover:bg-black/5 hover:text-zinc-900 dark:text-zinc-400 dark:hover:bg-white/[.07] dark:hover:text-white">
                                    <x-client.icon.panel-left />
                                </button>
                            </flux:tooltip>
                            @php($chiefOfStaff = app(\App\Client\ChiefOfStaff::class)->slot())
                            @if ($chiefOfStaff['enabled'])
                                <x-client.rail-link :href="$chiefOfStaff['conversation_id'] !== null ? route('conversations.show', $chiefOfStaff['conversation_id']) : route('connect')"
                                                    icon="sparkles" :label="$chiefOfStaff['name']" exact data-test="nav-chief-of-staff" />
                            @endif
                            <x-client.rail-link :href="route('conversations.index')" icon="chat-bubble-left-right" :label="__('Conversations')" exact />
                            <x-client.rail-link :href="route('lanes.index')" icon="code-bracket-square" :label="__('Task lanes')" />
                            <x-client.rail-link :href="route('intents.index')" icon="queue-list" :label="__('Tasks')" />
                            <x-client.rail-link :href="route('projects.index')" icon="folder" :label="__('Projects')" />
                            <x-client.rail-link :href="route('approvals')" icon="shield-check" :label="__('Approvals')" data-test="nav-approvals" />
                            <div class="my-1.5 h-px w-6 bg-black/10 dark:bg-white/10"></div>
                            <x-client.rail-link :href="route('fleet')" icon="server-stack" :label="__('Machines')" />
                            <x-client.rail-link :href="route('usage')" icon="chart-bar" :label="__('Usage')" />
                            <x-client.rail-link :href="route('sources')" icon="banknotes" :label="__('Source budgets')" />
                            <div class="flex-1"></div>
                            <x-client.rail-link :href="route('devices')" icon="device-phone-mobile" :label="__('Devices')" data-test="nav-devices" />
                            <x-client.rail-link :href="route('connect')" icon="cog-6-tooth" :label="__('Settings')" />
                        </nav>

                        <aside x-show="panel || drawer" class="flex w-64 shrink-0 flex-col border-e border-black/[.06] bg-cs-shell-light dark:border-white/[.06] dark:bg-cs-shell" data-test="sidebar-panel">
                            <div class="flex h-12 shrink-0 items-center gap-1 ps-3 pe-2">
                                <a href="{{ route('home') }}" wire:navigate class="me-auto text-[15px] font-semibold tracking-tight">ContextStack</a>
                                <flux:tooltip :content="__('Hide sidebar')">
                                    <flux:button size="sm" variant="ghost" square x-on:click="drawer ? drawer = false : togglePanel()" :aria-label="__('Hide sidebar')" data-test="sidebar-hide">
                                        <x-client.icon.panel-left />
                                    </flux:button>
                                </flux:tooltip>
                            </div>
                            <div class="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
                                <livewire:client.thread-rail />
                            </div>
                        </aside>
                    </div>
                </div>
            @endpersist

            <main class="flex min-w-0 flex-1 flex-col">
                <div class="flex h-12 shrink-0 items-center gap-2 px-3 md:hidden" data-test="mobile-bar">
                    <flux:button size="sm" variant="ghost" square x-data x-on:click="$dispatch('cs-sidebar')" :aria-label="__('Show sidebar')">
                        <x-client.icon.panel-left />
                    </flux:button>
                    <span class="text-[15px] font-semibold tracking-tight">ContextStack</span>
                </div>
                <div class="min-h-0 flex-1 overflow-y-auto px-4 py-6 md:px-8 has-[[data-full-bleed]]:overflow-hidden has-[[data-full-bleed]]:p-0">
                    {{ $slot }}
                </div>
            </main>
        </div>

        @persist('toast')
            <flux:toast.group>
                <flux:toast />
            </flux:toast.group>
        @endpersist

        @fluxScripts
    </body>
</html>
