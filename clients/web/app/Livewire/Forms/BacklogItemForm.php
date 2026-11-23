<?php

namespace App\Livewire\Forms;

use Livewire\Attributes\Validate;
use Livewire\Form;

/**
 * One backlog item's editable fields (CLIENT-API.md "Project backlog and heartbeat"): title, body, status, priority,
 * what blocks it and why, and whether an owner blocker stops the project's heartbeat. Links to intents, jobs and
 * decisions are written by agents and shown, not edited here.
 */
class BacklogItemForm extends Form
{
    public const array Statuses = ['next', 'now', 'blocked', 'done', 'dropped'];

    public const array BlockerKinds = ['owner', 'external', 'item', 'job'];

    #[Validate('required|string|max:500')]
    public string $title = '';

    #[Validate('nullable|string|max:20000')]
    public string $body = '';

    #[Validate('required|in:next,now,blocked,done,dropped')]
    public string $status = 'next';

    #[Validate('required|integer|between:-1000000000,1000000000')]
    public int|string $priority = 0;

    #[Validate('nullable|string|max:4000')]
    public string $blockedReason = '';

    #[Validate('boolean')]
    public bool $stop = false;

    /** @var list<array{kind: string, ref: string}> */
    #[Validate(['blockers' => 'array', 'blockers.*.kind' => 'required|in:owner,external,item,job', 'blockers.*.ref' => 'required|string|max:500'])]
    public array $blockers = [];

    /**
     * @param  array<string, mixed>  $item
     */
    public function fillFrom(array $item): void
    {
        $this->title = (string) ($item['title'] ?? '');
        $this->body = (string) ($item['body'] ?? '');
        $this->status = in_array($item['status'] ?? null, self::Statuses, true) ? (string) $item['status'] : 'next';
        $this->priority = (int) ($item['priority'] ?? 0);
        $this->blockedReason = (string) ($item['blocked_reason'] ?? '');
        $this->stop = (bool) ($item['stop'] ?? false);
        $this->blockers = [];
        foreach ((array) ($item['blocked_on'] ?? []) as $blocker) {
            if (is_array($blocker) && is_string($blocker['kind'] ?? null) && is_string($blocker['ref'] ?? null)) {
                $this->blockers[] = ['kind' => $blocker['kind'], 'ref' => $blocker['ref']];
            }
        }
    }

    public function addBlocker(): void
    {
        $this->blockers[] = ['kind' => 'owner', 'ref' => 'owner'];
    }

    public function removeBlocker(int $index): void
    {
        $this->blockers = array_values(array_filter($this->blockers, fn (int $position): bool => $position !== $index, ARRAY_FILTER_USE_KEY));
    }

    /**
     * The whole item as the Director takes it on create.
     *
     * @return array<string, mixed>
     */
    public function content(): array
    {
        $this->validate();

        return [
            'title' => trim($this->title),
            'body' => $this->body,
            'status' => $this->status,
            'priority' => (int) $this->priority,
            'blocked_reason' => $this->blockedReason,
            'stop' => $this->stop,
            'blocked_on' => array_map(fn (array $blocker): array => ['kind' => $blocker['kind'], 'ref' => trim($blocker['ref'])], $this->blockers),
        ];
    }

    /**
     * Only the fields that differ from the item as shown, for an update against its revision.
     *
     * @param  array<string, mixed>  $item
     * @return array<string, mixed>
     */
    public function changesFrom(array $item): array
    {
        $content = $this->content();
        $current = [
            'title' => (string) ($item['title'] ?? ''),
            'body' => (string) ($item['body'] ?? ''),
            'status' => (string) ($item['status'] ?? 'next'),
            'priority' => (int) ($item['priority'] ?? 0),
            'blocked_reason' => (string) ($item['blocked_reason'] ?? ''),
            'stop' => (bool) ($item['stop'] ?? false),
            'blocked_on' => array_values(array_map(fn ($blocker): array => ['kind' => (string) ($blocker['kind'] ?? ''), 'ref' => (string) ($blocker['ref'] ?? '')], array_filter((array) ($item['blocked_on'] ?? []), 'is_array'))),
        ];

        return array_filter($content, fn (mixed $value, string $field): bool => $value !== $current[$field], ARRAY_FILTER_USE_BOTH);
    }
}
