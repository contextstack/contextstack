<?php

declare(strict_types=1);

namespace App\Desktop;

use Native\Desktop\Contracts\MenuItem;
use Native\Desktop\Facades\Menu;

final class ApplicationMenu
{
    public const GETTING_STARTED_URL = 'https://github.com/contextstack/monorepo/blob/main/docs/release/getting-started.md';

    public const KNOWN_ISSUES_URL = 'https://github.com/contextstack/monorepo/blob/main/docs/release/alpha-known-issues.md';

    public static function register(): void
    {
        Menu::create(...self::items());
    }

    /**
     * Top-level menus are built with Menu::make()->label(), which sends no item type, so Electron makes them
     * submenus. Menu::label()->submenu() sends type "normal", which Electron keeps as a plain item and macOS leaves
     * out of the menu bar (only Edit showed).
     *
     * @return list<MenuItem>
     */
    public static function items(): array
    {
        $viewItems = [
            Menu::reload(),
            Menu::fullscreen(),
        ];

        if (! app()->isProduction()) {
            $viewItems[] = Menu::devTools();
        }

        return [
            Menu::make(
                Menu::about('About ContextStack'),
                Menu::separator(),
                Menu::hide(),
                Menu::separator(),
                Menu::quit(),
            )->label('ContextStack'),
            Menu::edit(),
            Menu::make(...$viewItems)->label('View'),
            Menu::make(
                Menu::minimize(),
                new ElectronMenuRole('zoom'),
            )->label('Window'),
            Menu::make(
                Menu::link(self::GETTING_STARTED_URL, 'Getting started')->openInBrowser(),
                Menu::link(self::KNOWN_ISSUES_URL, 'Known issues (alpha)')->openInBrowser(),
            )->label('Help'),
        ];
    }
}
