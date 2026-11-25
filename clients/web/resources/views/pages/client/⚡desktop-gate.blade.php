<?php

use App\Client\DesktopConnection;
use App\Client\DesktopConnectionStatus;
use App\Client\NativeDesktop;
use App\Desktop\ApplicationMenu;
use Livewire\Attributes\Layout;
use Livewire\Attributes\Title;
use Livewire\Component;

new #[Layout('layouts::desktop-gate')] #[Title('ContextStack')] class extends Component
{
    public DesktopConnectionStatus $status = DesktopConnectionStatus::NotSetUp;

    public function mount(DesktopConnection $desktop): void
    {
        abort_unless(NativeDesktop::isRunning(), 404);

        $this->refreshStatus($desktop);
    }

    public function tryAgain(DesktopConnection $desktop): void
    {
        $this->refreshStatus($desktop);

        if ($this->status === DesktopConnectionStatus::Ready) {
            $this->redirect(route('intents.index'), navigate: true);
        }
    }

    private function refreshStatus(DesktopConnection $desktop): void
    {
        $this->status = $desktop->status();
    }
};
?>

<section class="flex flex-col gap-6" data-test="desktop-gate">
    @if ($status === \App\Client\DesktopConnectionStatus::NotSetUp)
        <p class="text-6xl" aria-hidden="true">&#128736;</p>
        <flux:heading size="xl" level="1" data-test="desktop-gate-not-set-up">
            {{ __('ContextStack isn\'t set up on this Mac yet.') }}
        </flux:heading>
        <flux:text class="text-zinc-600 dark:text-zinc-300">
            {{ __('Install ContextStack on this Mac, or run') }}
            <code class="rounded bg-zinc-100 px-1.5 py-0.5 font-mono text-sm dark:bg-zinc-900">cs init</code>
            {{ __('if the CLI is already installed.') }}
        </flux:text>
        <flux:text>
            <a
                href="{{ ApplicationMenu::GETTING_STARTED_URL }}"
                data-native-external-link
                class="text-sky-500 underline underline-offset-2 hover:text-sky-400"
            >{{ __('Getting started (install)') }}</a>
        </flux:text>
        <flux:text class="text-zinc-600 dark:text-zinc-300">
            {{ __('ContextStack runs on another machine?') }}
            <a href="{{ route('connect') }}" class="text-sky-500 underline underline-offset-2 hover:text-sky-400" data-test="desktop-gate-pair">{{ __('Pair this Mac with its Director') }}</a>
        </flux:text>
    @elseif ($status === \App\Client\DesktopConnectionStatus::NotRunning)
        <p class="text-6xl" aria-hidden="true">&#128721;</p>
        <flux:heading size="xl" level="1" data-test="desktop-gate-not-running">
            {{ __('ContextStack isn\'t running.') }}
        </flux:heading>
        <flux:text class="text-zinc-600 dark:text-zinc-300">
            {{ __('In Terminal, run') }}
            <code class="rounded bg-zinc-100 px-1.5 py-0.5 font-mono text-sm dark:bg-zinc-900">cs status</code>
            {{ __('to see what is wrong, then') }}
            <code class="rounded bg-zinc-100 px-1.5 py-0.5 font-mono text-sm dark:bg-zinc-900">cs doctor</code>
            {{ __('to fix it.') }}
        </flux:text>
        <flux:text class="text-zinc-600 dark:text-zinc-300">
            {{ __('Connecting to a Director on another machine?') }}
            <a href="{{ route('connect') }}" class="text-sky-500 underline underline-offset-2 hover:text-sky-400" data-test="desktop-gate-settings">{{ __('Check its address in Settings') }}</a>
        </flux:text>
    @endif

    <div>
        <flux:button variant="primary" wire:click="tryAgain" data-test="desktop-gate-try-again">
            {{ __('Try again') }}
        </flux:button>
    </div>
</section>
