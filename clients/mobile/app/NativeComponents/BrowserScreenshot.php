<?php

namespace App\NativeComponents;

use App\Client\Json;
use App\Native\BrowserActions;
use App\Native\DirectorScreen;
use App\Native\ShowsBrowserScreenshot;
use Illuminate\View\View;

/**
 * A browser action's screenshot at full size, pushed from its approval. The route carries only the SHA; the Director
 * decides whether this credential may see it.
 */
class BrowserScreenshot extends DirectorScreen
{
    use ShowsBrowserScreenshot;

    public string $sha = '';

    public function mount(): void
    {
        $this->sha = Json::string($this->param('id'));
        $this->refresh();
    }

    public function refresh(): void
    {
        $this->loadScreenshot(['kind' => 'browser_action', 'screenshot_sha' => $this->sha]);
    }

    public function render(): View
    {
        return view('native.browser-screenshot', [
            'image' => BrowserActions::validSha($this->sha) ? $this->screenshotOf(['screenshot_sha' => $this->sha]) : '',
        ]);
    }
}
