<?php

namespace App\Native;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;

/**
 * The screenshot a browser action approval was proposed with (`GET browser-action-screenshots/{sha}`, owner only).
 * It is read once per SHA, checked to be a JPEG whose SHA-256 is the one the approval names, and kept only in this
 * screen's memory as a data URI for `native:image`: never in a public property (so never in the rendered state or a
 * log), never on disk. The Director accepts at most 3 MiB, so a larger answer is refused unread.
 *
 * @mixin DirectorScreen
 */
trait ShowsBrowserScreenshot
{
    /** The largest screenshot the Director stores (6 MiB of hex), in bytes. */
    private int $screenshotLimit = 3 * 1024 * 1024;

    /** The SHA the cached image or problem belongs to; '' when nothing is loaded. */
    private string $screenshotFor = '';

    private string $screenshotUri = '';

    /** Why the screenshot is not shown, or ''. */
    public string $screenshotProblem = '';

    /**
     * Read the approval's screenshot unless it is already loaded (or refused for good). A Director that could not
     * be reached is tried again on the next call.
     *
     * @param  array<string, mixed>  $approval
     */
    protected function loadScreenshot(array $approval): void
    {
        if (! BrowserActions::is($approval)) {
            return;
        }
        $sha = BrowserActions::sha($approval);
        if ($sha === '') {
            $this->forgetScreenshot('This request names no valid screenshot, so none is shown.');

            return;
        }
        if ($sha === $this->screenshotFor) {
            return;
        }

        try {
            $jpeg = $this->director()->bytes('browser-action-screenshots/'.$sha, 'image/jpeg', $this->screenshotLimit);
        } catch (DirectorUnavailable) {
            $this->forgetScreenshot('The screenshot could not be fetched: the Director could not be reached. Refresh to try again.');

            return;
        } catch (DirectorRefused $refused) {
            $this->forgetScreenshot(match ($refused->status) {
                403 => 'Only an owner can see the screenshot.',
                404 => 'The Director no longer has this screenshot.',
                default => 'The screenshot could not be read: '.$refused->detail,
            });
            if ($refused->status === 401) {
                $this->refused($refused);

                return;
            }
            // Refused for good: not asked again on every refresh.
            $this->screenshotFor = $sha;

            return;
        }

        $this->screenshotFor = $sha;
        if (! str_starts_with($jpeg, "\xFF\xD8\xFF") || hash('sha256', $jpeg) !== $sha) {
            $this->screenshotUri = '';
            $this->screenshotProblem = 'The screenshot the Director sent does not match the one this request was proposed with, so it is not shown.';

            return;
        }
        $this->screenshotUri = 'data:image/jpeg;base64,'.base64_encode($jpeg);
        $this->screenshotProblem = '';
    }

    /**
     * The loaded screenshot of this approval as a data URI, or ''.
     *
     * @param  array<string, mixed>  $approval
     */
    public function screenshotOf(array $approval): string
    {
        $sha = BrowserActions::sha($approval);

        return $sha !== '' && $sha === $this->screenshotFor ? $this->screenshotUri : '';
    }

    private function forgetScreenshot(string $problem): void
    {
        $this->screenshotFor = '';
        $this->screenshotUri = '';
        $this->screenshotProblem = $problem;
    }
}
