<?php

namespace App\Native;

use App\Client\DirectorClient;
use App\Client\Exceptions\DirectorRefused;
use App\Client\Exceptions\DirectorUnavailable;
use App\Client\Exceptions\NotConnected;
use Closure;
use Illuminate\Support\Facades\Log;
use Native\Mobile\Attributes\On;
use Native\Mobile\Edge\NativeComponent;
use Native\Mobile\Events\App\AppForegrounded;

/**
 * A screen that reads the Director. `attempt()` turns the Client API's failures into one line the screen can show,
 * so no screen renders a stack trace or a spinner that never ends. With the app lock on, a screen that is showing
 * when the app comes back after long enough away gives way to the locked screen.
 */
abstract class DirectorScreen extends NativeComponent
{
    public string $problem = '';

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

    protected function director(): DirectorClient
    {
        return app(DirectorClient::class);
    }

    /**
     * @template T
     *
     * @param  Closure(DirectorClient): T  $call
     * @return T|null
     */
    protected function attempt(Closure $call): mixed
    {
        try {
            $answer = $call($this->director());
            $this->problem = '';

            return $answer;
        } catch (NotConnected) {
            $this->replace('/connect');
        } catch (DirectorRefused $refused) {
            $this->problem = $refused->explain();
        } catch (DirectorUnavailable $unavailable) {
            Log::warning('The Director could not be reached.', ['error' => $unavailable->getMessage()]);
            $this->problem = 'The Director could not be reached. Check that Tailscale is on, then pull to refresh.';
        }

        return null;
    }
}
