<?php

namespace App\Http\Controllers\Client;

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
