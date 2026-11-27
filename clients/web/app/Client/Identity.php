<?php

namespace App\Client;

use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use Illuminate\Support\Facades\Cache;
use Throwable;

/**
 * Who this device is to its Director (`GET me`, CLIENT-API.md §2): the credential, the member it acts for, the
 * member's current role and the capabilities the Director advertises. The one place pages ask what to show.
 *
 * Capabilities come from the Director, never from the role's name. A Director from before teams answers `me`
 * without `capabilities`: every device was an owner then, so everything is shown as before. The Director stays
 * the authority: a control shown here can still be refused, and the page then refreshes (DirectorPage::attempt).
 *
 * The answer is kept for a short while per connection so each page render does not ask again. It is forgotten when
 * the connection changes, when the event stream ends or says `authorization_changed`, and on a 401, 403 or 404.
 */
class Identity
{
    /** Seconds one `me` answer is kept. */
    public const int KEEP = 30;

    public const string Unauthenticated = 'unauthenticated';

    public const string Unavailable = 'unavailable';

    public const string Known = 'known';

    /**
     * This request's answers, by connection key: asked at most once per request even when the Director cannot be
     * reached, so one page render never waits on `me` more than once (the binding is scoped to the request).
     *
     * @var array<string, array{status: string, me: array<string, mixed>}>
     */
    private array $asked = [];

    public function __construct(private readonly ConnectionStore $connections) {}

    /**
     * The `me` answer, or [] when it could not be read (see status()).
     *
     * @return array<string, mixed>
     */
    public function me(): array
    {
        return $this->answer()['me'];
    }

    /**
     * known (me was read), unauthenticated (the Director refused this credential with 401) or unavailable (not
     * connected, unreachable, or another refusal).
     */
    public function status(): string
    {
        return $this->answer()['status'];
    }

    public function unauthenticated(): bool
    {
        return $this->status() === self::Unauthenticated;
    }

    /**
     * A Director from before teams: `me` carries no capabilities.
     */
    public function legacy(): bool
    {
        $me = $this->me();

        return $me !== [] && ! is_array($me['capabilities'] ?? null);
    }

    /**
     * Whether the Director advertises this capability for this credential. Eligibility only: the Director still checks
     * the record. Without `me` (unreachable) or with a legacy `me`, controls show as before and the Director decides.
     */
    public function can(string $capability): bool
    {
        if ($this->unauthenticated()) {
            return false;
        }

        $capabilities = $this->me()['capabilities'] ?? null;
        if (! is_array($capabilities)) {
            return true;
        }

        return in_array($capability, $capabilities, true);
    }

    public function role(): ?string
    {
        $role = $this->me()['role'] ?? null;

        return is_string($role) && $role !== '' ? $role : null;
    }

    public function memberId(): ?string
    {
        $member = $this->me()['member'] ?? null;
        $id = is_array($member) ? ($member['id'] ?? null) : null;

        return is_string($id) && $id !== '' ? $id : null;
    }

    public function displayName(): ?string
    {
        $member = $this->me()['member'] ?? null;
        $name = is_array($member) ? ($member['display_name'] ?? null) : null;

        return is_string($name) && $name !== '' ? $name : null;
    }

    public function isMe(mixed $memberId): bool
    {
        return is_string($memberId) && $memberId !== '' && $memberId === $this->memberId();
    }

    /**
     * Ask again on the next read (a new connection, a change of role, a refusal, a dropped stream).
     */
    public function forget(): void
    {
        $this->asked = [];
        $key = $this->key();
        if ($key !== null) {
            Cache::forget($key);
        }
    }

    /**
     * @return array{status: string, me: array<string, mixed>}
     */
    private function answer(): array
    {
        $key = $this->key();
        if ($key === null) {
            return ['status' => self::Unavailable, 'me' => []];
        }

        if (isset($this->asked[$key])) {
            return $this->asked[$key];
        }

        $kept = Cache::get($key);
        if (is_array($kept) && is_string($kept['status'] ?? null) && is_array($kept['me'] ?? null)) {
            /** @var array{status: string, me: array<string, mixed>} $kept */
            return $this->asked[$key] = $kept;
        }

        $answer = $this->fetch();
        if ($answer['status'] !== self::Unavailable) {
            Cache::put($key, $answer, self::KEEP);
        }

        return $this->asked[$key] = $answer;
    }

    /**
     * @return array{status: string, me: array<string, mixed>}
     */
    private function fetch(): array
    {
        try {
            $me = app(DirectorClient::class)->get('me');
        } catch (DirectorRefused $refused) {
            return ['status' => $refused->status === 401 ? self::Unauthenticated : self::Unavailable, 'me' => []];
        } catch (DirectorUnavailable|NotConnected) {
            return ['status' => self::Unavailable, 'me' => []];
        } catch (Throwable) {
            // Identity is drawn into every page frame: an answer it cannot read leaves the page as it was before teams.
            return ['status' => self::Unavailable, 'me' => []];
        }

        return ['status' => self::Known, 'me' => $me];
    }

    /**
     * The cache key for the current connection: its URL and a hash of its credential (the token is never stored).
     */
    private function key(): ?string
    {
        try {
            $connection = $this->connections->resolve();
        } catch (NotConnected) {
            return null;
        }

        return 'client-identity:'.hash('sha256', $connection->url."\0".$connection->token);
    }
}
