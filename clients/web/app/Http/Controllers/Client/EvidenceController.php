<?php

namespace App\Http\Controllers\Client;

use App\Client\BrowserApproval;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorUnavailable;
use App\Http\Controllers\Controller;
use Illuminate\Http\Response;

/**
 * A run's evidence or a job's bundle file, fetched from the Director and passed on as the Director served it
 * (CLIENT-API.md §5): never as a page. Only the three content types the API uses are passed on, and every answer
 * carries nosniff, a sandboxing CSP and the file's sha256.
 */
class EvidenceController extends Controller
{
    public function run(DirectorClient $director, string $run, string $path): Response
    {
        return $this->relay($director, 'runs', $run, $path);
    }

    public function job(DirectorClient $director, string $job, string $path): Response
    {
        return $this->relay($director, 'jobs', $job, $path);
    }

    /**
     * A browser action approval's screenshot (CLIENT-API.md "Action approvals"), fetched with this device's Director
     * credential so the page never holds it, and passed on only as a JPEG that is never cached. The route accepts
     * only a lowercase sha256; a Director 404 or 403 is a plain 404 here.
     */
    public function browserScreenshot(DirectorClient $director, string $sha): Response
    {
        abort_unless(BrowserApproval::validSha($sha), 404);

        try {
            $answer = $director->browserScreenshot($sha);
        } catch (DirectorUnavailable) {
            abort(502, 'The Director could not be reached.');
        }

        abort_if(in_array($answer->status(), [403, 404], true), 404);
        $type = strtolower(trim(explode(';', (string) $answer->header('Content-Type'))[0]));
        abort_unless($answer->status() === 200 && $type === 'image/jpeg' && str_starts_with($answer->body(), "\xFF\xD8\xFF"), 502, 'The Director did not send a JPEG.');

        return response($answer->body(), 200, [
            'Content-Type' => 'image/jpeg',
            'Cache-Control' => 'no-store, private',
            'X-Content-Type-Options' => 'nosniff',
            'Content-Security-Policy' => 'sandbox',
        ]);
    }

    private function relay(DirectorClient $director, string $kind, string $id, string $path): Response
    {
        try {
            $answer = $director->file($kind, $id, $path);
        } catch (DirectorUnavailable) {
            abort(502, 'The Director could not be reached.');
        }

        abort_if($answer->status() === 404, 404);
        abort_unless($answer->status() === 200, 502, 'The Director refused the file.');

        $type = strtolower(trim(explode(';', (string) $answer->header('Content-Type'))[0]));
        $contentType = match ($type) {
            'application/json' => 'application/json',
            'text/plain' => 'text/plain; charset=utf-8',
            default => 'application/octet-stream',
        };

        $headers = [
            'Content-Type' => $contentType,
            'X-Content-Type-Options' => 'nosniff',
            'Content-Security-Policy' => 'sandbox',
            'ContextStack-Sha256' => hash('sha256', $answer->body()),
        ];
        if ($contentType === 'application/octet-stream') {
            $headers['Content-Disposition'] = 'attachment; filename="'.addcslashes(basename($path), '"\\').'"';
        }

        return response($answer->body(), 200, $headers);
    }
}
