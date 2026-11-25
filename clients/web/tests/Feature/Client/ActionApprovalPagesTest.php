<?php

use Illuminate\Http\Client\Request;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;

beforeEach(fn () => connectClientWeb());

/**
 * An action approval as the Director shows it (CLIENT-API.md "Action approvals").
 *
 * @return array<string, mixed>
 */
function approvalRecord(array $over = []): array
{
    return [...[
        'id' => 'apr_1', 'job_id' => 'job_lane', 'run_id' => 'run_lane', 'status' => 'needs_owner',
        'argv' => ['git', 'push', 'origin', 'lane/fix it'], 'needs' => ['network'], 'write_paths' => [],
        'reason' => 'Push the lane branch for review.', 'requesting_model' => 'codex',
        'reviewer_model' => 'local-coder', 'verdict' => 'no_go', 'review_reason' => 'Pushing leaves the machine.',
        'created_at' => '2026-11-21T10:00:00Z', 'outcome' => ['status' => 'waiting_on_owner'],
    ], ...$over];
}

/**
 * A running lane, with its job and intent, as the lane page fetches them.
 *
 * @return array<string, mixed>
 */
function approvalLane(): array
{
    $job = ['job_id' => 'job_lane', 'intent_id' => 'int_lane', 'node' => 'laptop', 'state' => 'running', 'created_at' => '2026-11-21T09:59:00Z',
        'live_run_ids' => [], 'run_id' => null, 'run_ids' => [], 'revision' => 2];

    return [
        director('jobs/job_lane') => Http::response($job),
        director('intents/int_lane') => Http::response(['intent' => ['intent_id' => 'int_lane', 'title' => 'Fix the widget', 'node' => 'laptop', 'task' => 'tasks/fix',
            'meta' => ['source' => 'client-web-lane'], 'lane' => true, 'created_at' => '2026-11-21T09:59:00Z', 'revision' => 1], 'jobs' => [$job], 'runs' => []]),
    ];
}

it('shows what waits on the owner with the exact command, both reasons and the verdict', function () {
    Http::fake([director('action-approvals') => Http::response(['action_approvals' => [
        approvalRecord(),
        approvalRecord(['id' => 'apr_2', 'status' => 'approved', 'verdict' => 'go', 'review_reason' => 'Fine.', 'outcome' => ['status' => 'succeeded', 'exit_code' => 0]]),
    ]])]);

    Livewire::test('pages::client.approvals')
        ->assertSeeInOrder(['Waiting on you', "git push origin 'lane/fix it'", 'network', 'Push the lane branch for review.', 'codex', 'no-go', 'Pushing leaves the machine.', 'local-coder', 'Approve', 'Deny'])
        ->assertSeeInOrder(['Recent', 'Approved by the reviewer', 'It ran and succeeded.']);
});

it('shows an approval that expired when its job ended as finished, not waiting', function () {
    Http::fake([director('action-approvals') => Http::response(['action_approvals' => [
        approvalRecord(['status' => 'expired', 'review_reason' => 'job ended', 'outcome' => null]),
    ]])]);

    Livewire::test('pages::client.approvals')
        ->assertSeeInOrder(['Recent', 'Expired', 'Its job ended before it was decided, so it did not run.'])
        ->assertDontSee('Approve');
});

it('says when no reviewer model was selected or called', function () {
    $record = approvalRecord();
    unset($record['reviewer_model']);
    Http::fake([director('action-approvals') => Http::response(['action_approvals' => [$record]])]);

    Livewire::test('pages::client.approvals')->assertSee('no reviewer model was selected or called');
});

it('approves a waiting request and shows its latest record', function () {
    Http::fake([
        director('action-approvals') => Http::response(['action_approvals' => [approvalRecord()]]),
        director('action-approvals/apr_1/decide') => Http::response(approvalRecord(['status' => 'approved', 'decided_by' => 'cred_local', 'decided_at' => '2026-11-21T10:05:00Z', 'outcome' => null])),
        director('action-approvals/apr_1') => Http::response(approvalRecord(['status' => 'approved', 'decided_by' => 'cred_local', 'decided_at' => '2026-11-21T10:05:00Z', 'outcome' => null])),
    ]);

    Livewire::test('pages::client.approvals')
        ->call('decideApproval', 'apr_1', 'approve')
        ->assertSet('problem', '')
        ->assertSee('Approved by you')
        ->assertSee('Nothing waits on you.');

    Http::assertSent(fn (Request $request) => $request->method() === 'POST' && $request->url() === director('action-approvals/apr_1/decide')
        && $request['decision'] === 'approve');
});

it('explains a competing decision and shows the record as it now is', function () {
    Http::fake([
        director('action-approvals') => Http::response(['action_approvals' => [approvalRecord()]]),
        director('action-approvals/apr_1/decide') => Http::response(['error' => 'Conflict', 'code' => 'conflict', 'detail' => 'the action approval is no longer in the required state'], 409),
        director('action-approvals/apr_1') => Http::response(approvalRecord(['status' => 'denied', 'decided_by' => 'cred_local', 'outcome' => ['status' => 'denied']])),
    ]);

    Livewire::test('pages::client.approvals')
        ->call('decideApproval', 'apr_1', 'deny')
        ->assertSee('This request was already decided')
        ->assertSee('It did not run.');
});

it('decides nothing for an approval the page does not hold or an unknown decision', function () {
    Http::fake([director('action-approvals') => Http::response(['action_approvals' => [approvalRecord()]])]);

    Livewire::test('pages::client.approvals')
        ->call('decideApproval', 'apr_other', 'approve')
        ->call('decideApproval', 'apr_1', 'maybe');

    Http::assertNotSent(fn (Request $request) => str_ends_with($request->url(), '/decide'));
});

it('keeps a lane page whole when the Director has no approvals for it', function () {
    Http::fake([
        ...approvalLane(),
        director('action-approvals*') => Http::response(['error' => 'Not Found', 'code' => 'not_found'], 404),
    ]);

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])->assertOk()->assertSet('problem', '');
});

it('puts a waiting request first on its lane page', function () {
    Http::fake([
        ...approvalLane(),
        director('action-approvals?job=job_lane') => Http::response(['action_approvals' => [approvalRecord()]]),
    ]);

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->assertSee('An agent asks to go beyond its sandbox')
        ->assertSee('Approve');
});

it('fetches an approval the stream says changed, once per batch, and shows a new request as it is made', function () {
    Http::fake([
        director('action-approvals') => Http::response(['action_approvals' => []]),
        director('action-approvals/apr_1') => Http::response(approvalRecord()),
    ]);
    $change = ['kind' => 'action_approval', 'id' => 'apr_1', 'revision' => 9, 'record' => ['id' => 'apr_1', 'job_id' => 'job_lane', 'revision' => 9]];

    Livewire::test('pages::client.approvals')
        ->assertSee('Nothing waits on you.')
        ->call('applyStream', [$change, [...$change, 'revision' => 10]])
        ->assertSee("git push origin 'lane/fix it'")
        ->assertSee('Approve');

    Http::assertSentCount(2);
});

it('fetches on a lane page only the approvals of its own job', function () {
    Http::fake([
        ...approvalLane(),
        director('action-approvals?job=job_lane') => Http::response(['action_approvals' => []]),
        director('action-approvals/apr_1') => Http::response(approvalRecord()),
        director('action-approvals/apr_other') => Http::response(approvalRecord(['id' => 'apr_other', 'job_id' => 'job_other'])),
    ]);

    Livewire::test('pages::client.lane', ['job' => 'job_lane'])
        ->call('applyStream', [
            ['kind' => 'action_approval', 'id' => 'apr_other', 'revision' => 3, 'record' => ['id' => 'apr_other', 'job_id' => 'job_other', 'revision' => 3]],
            ['kind' => 'action_approval', 'id' => 'apr_1', 'revision' => 4, 'record' => ['id' => 'apr_1', 'job_id' => 'job_lane', 'revision' => 4]],
        ])
        ->assertSee('An agent asks to go beyond its sandbox');

    Http::assertNotSent(fn (Request $request) => $request->url() === director('action-approvals/apr_other'));
});
