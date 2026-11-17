<?php

use App\Http\Controllers\Client\EventStreamController;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Carbon;
use Illuminate\Support\Facades\Http;
use Illuminate\Support\Facades\Log;

beforeEach(fn () => connectClientWeb());

it('passes evidence on as the Director typed it, never as a page', function (string $directorType, string $servedType) {
    Http::fake([director('runs/run_1/files/*') => Http::response('<html><script>alert(1)</script>', 200, ['Content-Type' => $directorType])]);

    $response = $this->get(route('runs.files', ['run' => 'run_1', 'path' => 'stimulus/a b.md']));

    $response->assertOk()
        ->assertHeader('Content-Type', $servedType)
        ->assertHeader('X-Content-Type-Options', 'nosniff')
        ->assertHeader('ContextStack-Sha256', hash('sha256', '<html><script>alert(1)</script>'));
    expect($response->headers->all('content-security-policy'))->toBe(['sandbox', "img-src 'self' data:"]);
    Http::assertSent(fn (Request $request) => $request->url() === director('runs/run_1/files/stimulus/a%20b.md'));
})->with([
    'text' => ['text/plain; charset=utf-8', 'text/plain; charset=utf-8'],
    'json' => ['application/json', 'application/json'],
    'html is not passed on' => ['text/html', 'application/octet-stream'],
]);

it('answers 404 for evidence the Director does not list, and 502 when it cannot be reached', function () {
    Http::fake([director('runs/run_1/files/missing.md') => Http::response(['code' => 'not_found'], 404)]);
    $this->get(route('runs.files', ['run' => 'run_1', 'path' => 'missing.md']))->assertNotFound();

    Http::fake(fn () => throw new ConnectionException('down'));
    $this->get(route('jobs.files', ['job' => 'job_1', 'path' => 'spec.yaml']))->assertStatus(502);
});

it('relays the Director\'s event stream with this device\'s credential, so the browser never holds it', function () {
    $frames = "retry: 2000\n\nevent: hello\ndata: {\"revision\": 41}\n\nid: 42\nevent: change\ndata: {\"kind\": \"job\", \"id\": \"job_1\", \"revision\": 42, \"record\": {}}\n\n";
    Http::fake([director('events') => Http::response($frames, 200, ['Content-Type' => 'text/event-stream'])]);

    $response = $this->get(route('stream'));

    $response->assertOk()->assertHeader('Content-Type', 'text/event-stream; charset=utf-8');
    expect($response->streamedContent())->toBe($frames);
    Http::assertSent(fn (Request $request) => $request->url() === director('events')
        && $request->hasHeader('Authorization', 'Bearer csl_local_test') && $request->hasHeader('Accept', 'text/event-stream'));
});

it('ends the relayed stream with a director_error frame when the Director refuses it', function () {
    Http::fake([director('events') => Http::response(['error' => 'Unauthorized', 'code' => 'unauthenticated', 'detail' => 'revoked'], 401)]);

    $content = $this->get(route('stream'))->streamedContent();

    expect($content)->toStartWith("event: director_error\n")->toContain('"code":"unauthenticated"');
});

it('keeps each relay inside PHP\'s time limit instead of lifting it', function (int $configured, int $maxExecution, int $seconds) {
    expect(EventStreamController::seconds($configured, $maxExecution))->toBe($seconds);
})->with([
    'php -S default' => [300, 30, 25],
    'no limit' => [300, 0, 300],
    'configured is sooner' => [20, 30, 20],
    'a tight limit' => [300, 8, 4],
    'configured floor' => [1, 0, 5],
]);

it('ends a relay that ran its time with a short retry and a relay_end frame, and logs nothing', function () {
    Log::spy();
    config(['client.stream_seconds' => 25]);
    $frames = "retry: 2000\n\nevent: hello\ndata: {\"revision\": 41}\n\n";
    Http::fake([director('events') => function () use ($frames) {
        Carbon::setTestNow(now()->addSeconds(25)); // the Director's stream ran the relay's whole time

        return Http::response($frames, 200, ['Content-Type' => 'text/event-stream']);
    }]);

    $content = $this->get(route('stream'))->streamedContent();

    expect($content)->toBe($frames."retry: 250\nevent: relay_end\ndata: {\"reason\":\"time\"}\n\n");
    Log::shouldNotHaveReceived('error');
    Carbon::setTestNow();
});
