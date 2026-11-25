<?php

namespace App\Providers;

use App\Desktop\ApplicationMenu;
use Native\Desktop\Contracts\ProvidesPhpIni;
use Native\Desktop\Facades\Window;

/**
 * ContextStack desktop: client-web in a native window. It talks to the local Director
 * (config/client.php: 127.0.0.1:8780 and the owner's local credential), so a machine set up
 * with `cs init` connects without pairing; a Mac without one pairs from Settings.
 *
 * The window never navigates to another host: it has no address bar and keeps NativePHP's preload, so a foreign
 * page there could pass for the app. Off-site links open in the browser (native-external-links.js).
 */
class NativeAppServiceProvider implements ProvidesPhpIni
{
    public function boot(): void
    {
        Window::open()
            ->title('ContextStack')
            ->width(1280)
            ->height(860)
            ->minWidth(900)
            ->minHeight(600)
            ->showDevTools(false)
            ->preventLeaveDomain()
            ->rememberState();

        ApplicationMenu::register();
    }

    /**
     * @return array<string, string>
     */
    public function phpIni(): array
    {
        return [
            'memory_limit' => '512M',
        ];
    }
}
