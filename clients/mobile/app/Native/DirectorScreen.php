<?php

namespace App\Native;

use App\Client\ConnectionStore;
use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Exceptions\TokenUnreadable;
use App\Client\Identity;
use App\Client\IdentityStore;
use App\Client\Json;
use Closure;
use Illuminate\Support\Facades\Log;
use Native\Mobile\Attributes\On;
use Native\Mobile\Edge\NativeComponent;
use Native\Mobile\Events\App\AppForegrounded;
use ReflectionClass;
use ReflectionProperty;

/**
 * A screen that reads the Director. `attempt()` turns the Client API's failures into one line the screen can show,
 * so no screen renders a stack trace or a spinner that never ends. With the app lock on, a screen that is showing
 * when the app comes back after long enough away gives way to the locked screen.
 *
 * Controls are offered from the capabilities `GET me` advertises (`can()`), never from the role's name. A 401 clears
 * what the screen shows and offers to pair again or forget the Director; a 403 or 404 reads `me` again, since the
 * member's role or an assignment may have changed (docs/design/teams-step2.md, "Both clients").
 */
abstract class DirectorScreen extends NativeComponent
{
    public string $problem = '';

    /**
     * One Idempotency-Key per action until the Director has answered it: an answer lost on the way may still have
     * been recorded, so trying again must replay the same request, not make a second one (CLIENT-API.md §7.1).
     *
     * @var array<string, string> action => key
     */
    public array $actionKeys = [];

    /** The Director no longer accepts this device's credential (401): nothing from it is shown. */
    public bool $signedOut = false;

    /**
     * What a sign-out keeps: the route's record IDs and how the screen was opened, so its back button and a later
     * reconnect still work. Everything else read from the Director goes back to its default.
     */
    public const array KeptOnSignOut = ['conversationId', 'jobId', 'projectId', 'linked', 'thisDevice', 'sha'];

    /**
     * The app's own listener has already decided whether this return locks it (Laravel's listeners run before the
     * live screen hears the event).
     */
    #[On(AppForegrounded::class)]
    public function returnedToApp(int $secondsAway = 0): void
    {
        if (app(AppLock::class)->locked()) {
            $this->replace('/locked');
        }
    }

    protected function keyFor(string $action): string
    {
        return $this->actionKeys[$action] ??= DirectorClient::newKey();
    }

    /**
     * The Director answered the action (recorded it or refused it), so a new attempt is a new request.
     */
    protected function answered(string $action): void
    {
        unset($this->actionKeys[$action]);
    }

    protected function director(): DirectorClient
    {
        return app(DirectorClient::class);
    }

    /**
     * Who this credential acts for, or null when the Director could not say just now.
     */
    public function identity(): ?Identity
    {
        return app(IdentityStore::class)->current();
    }

    /**
     * Whether to offer an action: the Director advertises the capability (or predates capabilities). Its refusals
     * still win.
     */
    public function can(string $capability): bool
    {
        return app(IdentityStore::class)->can($capability);
    }

    /**
     * The words for a refusal, and what it means for the screen: a 401 signs the screen out; a 403 or 404 reads `me`
     * again before the screen refreshes.
     */
    protected function refused(DirectorRefused $refused): string
    {
        if ($refused->status === 401) {
            $this->signOut();

            return $this->problem;
        }
        if (in_array($refused->status, [403, 404], true)) {
            app(IdentityStore::class)->forget();
        }

        return $refused->explain();
    }

    /**
     * The Director refused this credential: clear everything this screen read from it (each property back to its
     * default) and offer to pair again or forget.
     */
    protected function signOut(): void
    {
        app(IdentityStore::class)->forget();
        foreach ((new ReflectionClass($this))->getProperties(ReflectionProperty::IS_PUBLIC) as $property) {
            if ($property->getDeclaringClass()->getName() === static::class && ! $property->isStatic() && $property->hasDefaultValue()
                && ! in_array($property->getName(), self::KeptOnSignOut, true)) {
                $property->setValue($this, $property->getDefaultValue());
            }
        }
        $this->signedOut = true;
        $this->problem = 'The Director no longer accepts this device. It may have been revoked, or its member disabled. Pair again, or forget this Director.';
    }

    /**
     * A member's name: "you", their display name when this credential may list members, or their ID.
     */
    public function memberName(?string $memberId, ?string $displayName = null): string
    {
        if ($memberId === null || $memberId === '') {
            return $displayName ?? '';
        }
        if ($this->identity()?->isMe($memberId) === true) {
            return 'you';
        }

        return $displayName ?? app(IdentityStore::class)->memberNames()[$memberId] ?? $memberId;
    }

    /**
     * The name of a record's sponsor, responsible member or assignee: the `{role}_display_name` the Director sends
     * beside `{role}_member_id`, or the name looked up for the ID.
     *
     * @param  array<string, mixed>  $record
     */
    public function nameOn(array $record, string $role): string
    {
        return $this->memberName(Json::nullableString($record[$role.'_member_id'] ?? null), Json::nullableString($record[$role.'_display_name'] ?? null));
    }

    /**
     * What the record's own `actions` says about an action (`control`, `cancel`, `decide`, `answer`, `adopt`,
     * `archive`), or null when the Director does not send `actions`. When it is sent, it alone decides whether the
     * control shows.
     *
     * @param  array<string, mixed>  $record
     */
    public static function action(array $record, string $action): ?bool
    {
        $actions = $record['actions'] ?? null;

        return is_array($actions) ? ($actions[$action] ?? false) === true : null;
    }

    /**
     * Who a needs-you item waits on, from its `assignee_member_id`: "Needs you" when it is this member's (or the
     * Director predates assignment), otherwise "Assigned to …".
     *
     * @param  array<string, mixed>  $item
     */
    public function assignment(array $item): string
    {
        $assignee = Json::nullableString($item['assignee_member_id'] ?? null);
        $identity = $this->identity();
        if ($assignee === null || $identity === null || $identity->memberId === null || $identity->isMe($assignee)) {
            return 'Needs you';
        }

        return 'Assigned to '.$this->nameOn($item, 'assignee');
    }

    /**
     * Whether to offer the decision on an action or land approval: the credential may approve actions, and the item
     * is assigned to this member, or this is an owner, who may take over any eligible item (docs/design/teams-step2.md).
     *
     * @param  array<string, mixed>  $item
     */
    public function mayDecideApproval(array $item): bool
    {
        $told = self::action($item, 'decide');
        if ($told !== null) {
            return $told;
        }
        if (! $this->can('approve_action')) {
            return false;
        }
        $identity = $this->identity();
        $assignee = Json::nullableString($item['assignee_member_id'] ?? null);

        if ($identity === null || $identity->legacy()) {
            return $identity !== null;
        }
        // Only an owner decides a land, install, rollback or browser action approval.
        if (ActionApprovals::isLand($item) || ReleaseApprovals::isRelease($item) || BrowserActions::is($item)) {
            return $identity->owner;
        }

        return $identity->owner || $assignee === null || $identity->isMe($assignee);
    }

    /**
     * Whether to offer controls on a record's work (stop, go on, send, cancel, decide): a credential that manages
     * members may act on anyone's work; anyone else only on records attributed to them. A record without
     * `responsible_member_id` (a Director before teams) keeps the old behaviour. The Director decides.
     *
     * @param  array<string, mixed>  $record
     */
    public function mine(array $record): bool
    {
        $responsible = Json::nullableString($record['responsible_member_id'] ?? null);
        if ($responsible === null || $this->can('manage_members')) {
            return true;
        }

        return $this->identity()?->isMe($responsible) === true;
    }

    /**
     * Open a needs-you link in its screen here: an approval in Waiting, a job in its lane screen, a backlog item in
     * its project.
     */
    public function openLink(string $kind, string $id, string $projectId = ''): void
    {
        match ($kind) {
            'approval' => $this->navigate('/', ['approval' => $id]),
            'job' => $this->navigate('/lanes/'.rawurlencode($id)),
            'backlog' => $projectId !== '' ? $this->navigate('/projects/'.rawurlencode($projectId), ['item' => $id]) : null,
            default => null,
        };
    }

    public function reconnect(): void
    {
        $this->replace('/connect');
    }

    public function forgetDirector(): void
    {
        app(ConnectionStore::class)->forget();
        app(AppLock::class)->disable();
        app(IdentityStore::class)->forget();
        $this->replace('/connect');
    }

    /**
     * @template T
     *
     * @param  Closure(DirectorClient): T  $call
     * @return T|null
     */
    protected function attempt(Closure $call): mixed
    {
        app(IdentityStore::class)->retry();

        try {
            $answer = $call($this->director());
            $this->problem = '';

            return $answer;
        } catch (NotConnected) {
            $this->replace('/connect');
        } catch (DirectorRefused $refused) {
            $this->problem = $this->refused($refused);
        } catch (TokenUnreadable $unreadable) {
            $this->problem = $unreadable->getMessage();
        } catch (DirectorUnavailable $unavailable) {
            Log::warning('The Director could not be reached.', ['error' => $unavailable->getMessage()]);
            $this->problem = 'The Director could not be reached. Check that Tailscale is on, then pull to refresh.';
        }

        return null;
    }
}
