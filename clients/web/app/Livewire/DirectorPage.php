<?php

namespace App\Livewire;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use App\Client\Identity;
use App\Client\LiveUpdates;
use Livewire\Attributes\Locked;
use Livewire\Component;

/**
 * A client-web page that shows Director records and keeps them current from the event stream. The browser follows
 * the reconnect order (resources/js/director-stream.js) and calls refetch() then applyStream(); the page applies a
 * change only when its revision is newer than the record it holds (CLIENT-API.md §8 step 4).
 *
 * Records are held as "kind:id" => record, locked so a browser cannot set them directly. They are not a trusted copy:
 * applyStream() is a public action and stores whatever newer-revision record it is sent, so an action that acts on a
 * held record must not rely on it having come from the Director.
 */
abstract class DirectorPage extends Component
{
    /** @var array<string, array<string, mixed>> */
    #[Locked]
    public array $records = [];

    public string $problem = '';

    /**
     * The Director refused this device's credential (401): what the page held is cleared and it offers to reconnect
     * or forget this device instead (CLIENT-API.md §2).
     */
    #[Locked]
    public bool $signedOut = false;

    /** Set while a stale refusal is refreshing the page, so a refusal during that refresh does not refresh again. */
    private bool $refreshing = false;

    /** The action this request runs (set from Livewire's `call` hook in AppServiceProvider); null while mounting. */
    protected ?string $action = null;

    /** Actions that read rather than write: a refusal to one is not followed by a second read. */
    private const array Reads = ['refetch', 'applyStream', 'streamRefused', 'showMore', 'showEarlier'];

    /**
     * Livewire is about to run $method on this page.
     */
    public function calling(string $method): void
    {
        $this->action = $method;
    }

    /** The status of the last refusal attempt() saw, for this request only (not kept between requests). */
    protected ?int $refusedStatus = null;

    /**
     * Fetch the state this page shows (reconnect order, step 3).
     */
    abstract public function refetch(): void;

    /**
     * Whether this page shows a change to this record.
     *
     * @param  array<string, mixed>  $change
     */
    abstract protected function follows(array $change): bool;

    /**
     * Held and live events, in the order they arrived (reconnect order, step 4).
     *
     * @param  list<mixed>  $changes
     * @param  list<mixed>  $runEvents
     */
    public function applyStream(array $changes, array $runEvents = []): void
    {
        $applied = false;
        foreach ($changes as $change) {
            if (! is_array($change) || ! $this->follows($change)) {
                continue;
            }
            // A deleted record comes with no body: drop it.
            if (($change['deleted'] ?? false) === true && is_string($change['kind'] ?? null) && is_string($change['id'] ?? null)) {
                if (isset($this->records[$change['kind'].':'.$change['id']])) {
                    unset($this->records[$change['kind'].':'.$change['id']]);
                    $applied = true;
                }

                continue;
            }
            if (is_string($change['kind'] ?? null) && is_array($change['record'] ?? null)) {
                $change['record'] = $this->shown($change['kind'], $change['record']);
            }
            $next = LiveUpdates::applyChange($this->records, $change);
            if ($next !== null) {
                $this->records = $next;
                $applied = true;
            }
        }

        if ($applied) {
            $this->afterChanges();
        }

        $events = array_values(array_filter($runEvents, 'is_array'));
        if ($events !== []) {
            $this->applyRunEvents($events);
        }
    }

    public function streamRefused(string $detail): void
    {
        $this->problem = $detail;
    }

    /**
     * @param  list<array<string, mixed>>  $events
     */
    protected function applyRunEvents(array $events): void {}

    protected function afterChanges(): void {}

    /**
     * @param  array<string, mixed>  $record
     */
    protected function hold(string $kind, string $id, array $record): void
    {
        $this->records[$kind.':'.$id] = $this->shown($kind, $record);
    }

    /**
     * The part of a record this page holds, from a fetch or a stream change. Pages that hold many records keep only
     * what they show (see ListRecords), since held records travel with every Livewire request. The revision must
     * stay: stream changes are applied by it.
     *
     * @param  array<string, mixed>  $record
     * @return array<string, mixed>
     */
    protected function shown(string $kind, array $record): array
    {
        return $record;
    }

    /**
     * @return array<string, mixed>|null
     */
    protected function held(string $kind, string $id): ?array
    {
        return $this->records[$kind.':'.$id] ?? null;
    }

    /**
     * Every held record of one kind, newest revision last.
     *
     * @return list<array<string, mixed>>
     */
    protected function heldOf(string $kind): array
    {
        $found = [];
        foreach ($this->records as $key => $record) {
            if (str_starts_with($key, $kind.':')) {
                $found[] = $record;
            }
        }

        return $found;
    }

    /**
     * A usage answer without API-price estimates (`est_usd`), which are not charges and are never shown, not even in
     * the page state sent to the browser (CLIENT-API.md §12.4).
     *
     * @param  array<array-key, mixed>  $usage
     * @return array<array-key, mixed>
     */
    protected static function withoutEstimates(array $usage): array
    {
        unset($usage['est_usd']);
        foreach ($usage as $key => $value) {
            if (is_array($value)) {
                $usage[$key] = self::withoutEstimates($value);
            }
        }

        return $usage;
    }

    protected function director(): DirectorClient
    {
        return app(DirectorClient::class);
    }

    /**
     * Run a Director call, showing why it failed instead of failing the page.
     *
     * @template T
     *
     * @param  callable(): T  $call
     * @return T|null
     */
    protected function attempt(callable $call): mixed
    {
        try {
            $result = $call();
            $this->problem = '';

            return $result;
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();
            $this->refusedStatus = $refused->status;
            $this->refused($refused);
        } catch (DirectorUnavailable $unavailable) {
            $this->problem = $unavailable->getMessage();
        } catch (NotConnected $notConnected) {
            $this->problem = $notConnected->getMessage();
        }

        return null;
    }

    /**
     * What a refusal means for the page beyond its words. 401: the credential is gone, so nothing protected stays on
     * screen. 403 or 404: the role or the record changed under the page, so it asks who it is again; after a refused
     * write it also fetches what it shows (once), keeping the refusal's words. A refused read (mounting, refetch) is
     * not read again: that read was the fresh one.
     */
    protected function refused(DirectorRefused $refused): void
    {
        $identity = app(Identity::class);

        if ($refused->status === 401) {
            $identity->forget();
            $this->signOut();

            return;
        }

        if (! in_array($refused->status, [403, 404], true)) {
            return;
        }

        $identity->forget();
        if ($this->refreshing || $this->action === null || in_array($this->action, self::Reads, true)) {
            return;
        }

        $problem = $this->problem;
        $this->refreshing = true;
        try {
            $this->refetch();
        } finally {
            $this->refreshing = false;
        }
        $this->problem = $problem;
    }

    /**
     * Drop every held record: after a 401 nothing the Director showed this credential stays on screen.
     */
    protected function signOut(): void
    {
        $this->records = [];
        $this->signedOut = true;
        // The frame's banner (layouts/client) offers Settings, where this device reconnects or is forgotten.
        $this->dispatch('cs-signed-out');
        $this->problem = __('The Director no longer accepts this device\'s credential. It may have been revoked or its member disabled. Reconnect or forget this device in Settings.');
    }
}
