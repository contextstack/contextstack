<?php

namespace App\Client;

/**
 * A needs-you item as conversation attention carries it (CLIENT-API.md §11.7, "Action approvals", "Project backlog
 * and heartbeat"; docs/design/hosted-needs-you-client.md): the question, the options to press, the one to highlight,
 * links to the records it is about, who it is assigned to, and what an answer goes to (an action approval or a
 * backlog item). Also who may answer it on this device.
 */
final class NeedsYou
{
    /** Link keys the Director sends, in the order they are shown. */
    public const array LinkKinds = ['approval', 'job', 'run', 'lane', 'backlog'];

    /**
     * @return array{question: string, options: list<array{id: string, label: string}>, default: ?string, links: array<string, string>, approval: ?string, backlog: ?string, project: ?string, revision: int, assignee: ?string, assignee_name: ?string, kind: ?string, actions: array<string, mixed>|null}|null
     */
    public static function from(mixed $attention): ?array
    {
        if (! is_array($attention)) {
            return null;
        }

        $options = [];
        foreach ((array) ($attention['options'] ?? []) as $option) {
            if (is_array($option) && is_string($option['id'] ?? null) && $option['id'] !== '') {
                $options[] = ['id' => $option['id'], 'label' => is_string($option['label'] ?? null) && $option['label'] !== '' ? $option['label'] : $option['id']];
            }
        }

        $links = [];
        foreach (self::LinkKinds as $kind) {
            $id = $attention['links'][$kind] ?? null;
            if (is_string($id) && $id !== '') {
                $links[$kind] = $id;
            }
        }

        $approval = $links['approval'] ?? (is_string($attention['action_approval_id'] ?? null) && $attention['action_approval_id'] !== '' ? $attention['action_approval_id'] : null);

        return [
            'question' => is_string($attention['question'] ?? null) ? $attention['question'] : '',
            'options' => $options,
            'default' => is_string($attention['default'] ?? null) ? $attention['default'] : null,
            'links' => $links,
            'approval' => $approval,
            'backlog' => $links['backlog'] ?? null,
            'project' => is_string($attention['project_id'] ?? null) && $attention['project_id'] !== '' ? $attention['project_id'] : null,
            'revision' => is_int($attention['revision'] ?? null) ? $attention['revision'] : 0,
            'assignee' => is_string($attention['assignee_member_id'] ?? null) && $attention['assignee_member_id'] !== '' ? $attention['assignee_member_id'] : null,
            'assignee_name' => is_string($attention['assignee_display_name'] ?? null) && $attention['assignee_display_name'] !== '' ? $attention['assignee_display_name'] : null,
            // The approval's kind (land or an action), when the Director puts it on the item: no separate read needed.
            'kind' => is_string($attention['kind'] ?? null) && $attention['kind'] !== '' ? $attention['kind'] : null,
            'actions' => is_array($attention['actions'] ?? null) ? $attention['actions'] : null,
        ];
    }

    /**
     * The words over a needs-you item: "Needs you" when it is assigned to this member (or the Director assigns
     * nothing yet), else whom it is assigned to.
     *
     * @param  array<string, mixed>  $attention
     */
    public static function assignment(array $attention): string
    {
        return self::assignedElsewhere($attention) ?? __('Needs you');
    }

    /**
     * "Assigned to …" when the item is assigned to another member; null when it is this member's (or unassigned).
     *
     * @param  array<string, mixed>  $attention
     */
    public static function assignedElsewhere(array $attention): ?string
    {
        $assignee = $attention['assignee_member_id'] ?? null;
        if (! is_string($assignee) || $assignee === '' || app(Identity::class)->isMe($assignee)) {
            return null;
        }

        return __('Assigned to :name', ['name' => (string) app(MemberNames::class)->label($assignee, $attention['assignee_display_name'] ?? null)]);
    }

    /**
     * Whether this device may answer: the Director advertises the capability and the item's assignment allows it.
     * An owner may take over an assigned approval by deciding it; only an owner decides a land. The Director decides
     * in the end: a refusal shows the item as unavailable.
     *
     * When the item carries `actions`, its `answer` alone decides.
     *
     * @param  array{approval: ?string, backlog: ?string, assignee: ?string, kind?: ?string, actions?: array<string, mixed>|null}  $needs
     * @param  array<string, mixed>|null  $approval  the approval record, when the item is one; null while it is not loaded
     */
    public static function answerable(array $needs, ?array $approval = null): bool
    {
        return RecordActions::allows(['actions' => $needs['actions'] ?? null], 'answer', fn (): bool => self::answerableByRule($needs, $approval));
    }

    /**
     * Today's rule, for a Director that sends no `actions`.
     *
     * @param  array{approval: ?string, backlog: ?string, assignee: ?string, kind?: ?string}  $needs
     * @param  array<string, mixed>|null  $approval
     */
    private static function answerableByRule(array $needs, ?array $approval): bool
    {
        $identity = app(Identity::class);
        $owner = (bool) ($identity->me()['owner'] ?? false);

        if ($needs['approval'] !== null) {
            // Until the approval itself is read (or the item names its kind), it is not known to be a land (owner
            // only) or still open.
            $kind = $needs['kind'] ?? null;
            if ($kind === null && (! is_array($approval) || ($approval['status'] ?? null) !== 'needs_owner')) {
                return false;
            }
            if (is_array($approval) && ($approval['status'] ?? null) !== 'needs_owner') {
                return false;
            }
            if (in_array($approval['kind'] ?? $kind, ['land', BrowserApproval::Kind, ...ReleaseApproval::Kinds], true)) {
                return $identity->legacy() || $owner;
            }

            return $identity->can('approve_action') && ($identity->legacy() || $owner || $needs['assignee'] === null || $identity->isMe($needs['assignee']));
        }

        if ($needs['backlog'] !== null) {
            return $identity->can('edit_backlog');
        }

        return false;
    }

    /**
     * The in-app page for one link, or null when the app has none for it.
     */
    public static function href(string $kind, string $id, ?string $project = null): ?string
    {
        return match ($kind) {
            'job', 'lane' => route('lanes.show', $id),
            'run' => route('runs.show', $id),
            'approval' => route('approvals'),
            'backlog' => $project !== null ? route('backlog.show', ['project' => $project, 'item' => $id]) : null,
            default => null,
        };
    }
}
