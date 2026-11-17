<?php

use App\Client\Bundle;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use Illuminate\Http\Client\ConnectionException;
use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;

beforeEach(fn () => connectClientWeb());

it('sends every write with its Idempotency-Key and reports a replay', function () {
    Http::fake([director('runs/r1/decision') => Http::response(['decision_id' => 'dec_1'], 200, ['Idempotent-Replayed' => 'true'])]);

    $answer = app(DirectorClient::class)->write('runs/r1/decision', ['decision' => 'approve'], 'key-1');

    expect($answer->replayed)->toBeTrue()->and($answer->body['decision_id'])->toBe('dec_1');
    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->hasHeader('Idempotency-Key', 'key-1')
        && $request->hasHeader('Authorization', 'Bearer csl_local_test') && $request['decision'] === 'approve');
});

it('turns 409, 422 and 428 into refusals that carry the code and the body', function (int $status, string $code, array $extra) {
    Http::fake([director('intents') => Http::response(['error' => 'x', 'code' => $code, 'detail' => 'why', ...$extra], $status)]);

    try {
        app(DirectorClient::class)->write('intents', ['node' => 'laptop'], 'key-2');
        $this->fail('expected a refusal');
    } catch (DirectorRefused $refused) {
        expect($refused->status)->toBe($status)->and($refused->errorCode)->toBe($code)->and($refused->body)->toMatchArray($extra);
    }
})->with([
    'conflict' => [409, 'idempotency_conflict', ['record' => ['kind' => 'intent', 'id' => 'int_1']]],
    'invalid' => [422, 'invalid_request', []],
    'key required' => [428, 'idempotency_key_required', []],
]);

it('reports an unreachable Director as unavailable, so the caller keeps its key', function () {
    Http::fake(fn () => throw new ConnectionException('refused'));

    app(DirectorClient::class)->write('intents', [], 'key-3');
})->throws(DirectorUnavailable::class);

it('uploads every object before submitting the intent', function () {
    Http::fake([
        director('objects/*') => Http::response(['stored' => true]),
        director('intents') => Http::response(['intent' => ['intent_id' => 'int_1'], 'jobs' => [], 'runs' => []], 201),
    ]);
    $bundle = (new Bundle)->add('spec.yaml', "id: x\n")->add('tasks/t/goal.md', "Say done.\n");

    app(DirectorClient::class)->submitIntent(['node' => 'laptop', 'spec' => 'spec.yaml', 'task' => 'tasks/t'], $bundle, 'key-4');

    $order = Http::recorded()->map(fn (array $pair) => $pair[0]->method().' '.$pair[0]->url())->all();
    expect($order)->toBe([
        'PUT '.director('objects/'.hash('sha256', "id: x\n")),
        'PUT '.director('objects/'.hash('sha256', "Say done.\n")),
        'POST '.director('intents'),
    ]);
    Http::assertSent(fn (Request $request) => $request->method() === 'PUT' && $request->body() === "id: x\n");
    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request['files'] === $bundle->files());
});

it('uploads the objects the Director names as missing and sends the same request again with the same key', function () {
    $goal = "Say done.\n";
    Http::fake([
        director('objects/*') => Http::response(['stored' => false]),
        director('intents') => Http::sequence()
            ->push(['error' => 'x', 'code' => 'objects_missing', 'detail' => 'missing', 'missing' => [hash('sha256', $goal)]], 422)
            ->push(['intent' => ['intent_id' => 'int_1'], 'jobs' => [], 'runs' => []], 201),
    ]);
    $bundle = (new Bundle)->add('spec.yaml', "id: x\n")->add('tasks/t/goal.md', $goal);

    $answer = app(DirectorClient::class)->submitIntent(['node' => 'laptop'], $bundle, 'key-5');

    expect($answer->status)->toBe(201);
    $posts = Http::recorded()->filter(fn (array $pair) => $pair[0]->method() === 'POST')->values();
    expect($posts)->toHaveCount(2)
        ->and($posts->map(fn (array $pair) => $pair[0]->header('Idempotency-Key')[0])->unique()->all())->toBe(['key-5']);
    expect(Http::recorded()->filter(fn (array $pair) => $pair[0]->url() === director('objects/'.hash('sha256', $goal)))->count())->toBe(2);
});

it('percent-encodes each segment of an evidence path', function () {
    expect(DirectorClient::encodePath('stimulus/a b#1.md'))->toBe('stimulus/a%20b%231.md');
});
