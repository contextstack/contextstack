/**
 * Follows the Director's event stream for a Livewire page (tests/director-spec/CLIENT-API.md §8), through client-web's
 * relay, so the browser never holds the Director's credential.
 *
 * The reconnect order, also used at first load:
 *   1. open the stream and wait for `hello`;
 *   2. hold the `change` and `run_event` events that arrive from then on;
 *   3. refetch the page's state ($wire.refetch());
 *   4. apply the held events, then the live ones ($wire.applyStream()); the page applies a change only when its
 *      revision is newer than the record it holds, and live output only above the last seq shown.
 * When the stream drops, ends with `resync` or `revoked`, or ends any other way, start again at step 1.
 *
 * client-web's relay ends itself before PHP's time limit with `event: relay_end`: a planned end. The order is the same,
 * but quietly: the page stays "live" while it reconnects and refetches. A `retry:` field sets the wait before
 * reconnecting (the Director sends one at the start of its stream; the relay a short one before relay_end).
 *
 * One connection per stream URL, shared: the sidebar and the page on screen both follow `stream`, and a php -S
 * worker held per open stream is scarce (the desktop app has eight). Each scope still runs the order above on its
 * own: when the shared stream says `hello`, or when a scope joins a stream that is already live, that scope holds,
 * refetches and applies. The connection stays open across wire:navigate and closes a few seconds after its last
 * scope leaves.
 */
const RETRY_MS = 2000;
const FLUSH_MS = 250;
const LINGER_MS = 5000;
/** How soon after a page joins to look again for streams the replaced page left behind. */
const SWEEP_MS = 1000;

/** The shared connections, by URL. */
const streams = new Map();

/**
 * One connection to a stream URL and the scopes following it. Scopes are directorStream() components; the stream
 * calls their begin(), receive(), dropped() and refused().
 */
export class SharedStream {
    constructor(url, open = (signal) => fetch(url, { headers: { Accept: 'text/event-stream' }, credentials: 'same-origin', signal })) {
        this.url = url;
        this.open = open;
        this.scopes = new Set();
        this.running = false;
        this.live = false;
        this.refusedLast = false;
        this.retryMs = RETRY_MS;
        this.controller = null;
        this.lingerTimer = null;
        this.sweepTimer = null;
        this.retryTimer = null;
    }

    static for(url, open = undefined) {
        if (!streams.has(url)) {
            streams.set(url, new SharedStream(url, open));
        }

        return streams.get(url);
    }

    /**
     * Forget scopes whose element has left the page. wire:navigate does not always run Alpine's destroy() on the
     * page it replaces, and a scope that never leaves kept its stream reconnecting forever.
     */
    prune() {
        for (const scope of this.scopes) {
            if (scope.root && !scope.root.isConnected) {
                this.scopes.delete(scope);
            }
        }
    }

    /**
     * A page joining another URL means the page before it is gone: close every other stream nobody on screen
     * follows, so going from one conversation to the next does not hold one of the desktop server's few workers per
     * page. Run again shortly after the join, since the page being replaced is still attached while the new one starts.
     * Streams are told apart by URL: a stream reached through Alpine's reactive proxy is not the listed object.
     */
    closeOthers() {
        for (const other of streams.values()) {
            other.prune();
            if (other.url !== this.url && other.scopes.size === 0) {
                clearTimeout(other.lingerTimer);
                other.stop();
            }
        }
    }

    join(scope) {
        // A stream stopped and joined again (a page restored by wire:navigate) is listed again, so it can be closed.
        if (!streams.has(this.url)) {
            streams.set(this.url, this);
        }
        this.closeOthers();
        clearTimeout(this.sweepTimer);
        this.sweepTimer = setTimeout(() => this.closeOthers(), SWEEP_MS);
        clearTimeout(this.lingerTimer);
        this.scopes.add(scope);
        if (!this.running) {
            this.running = true;
            this.connect();
        } else if (this.live) {
            // Joining a stream that already said hello: this scope holds from now, refetches and applies.
            scope.begin();
        }
    }

    leave(scope) {
        this.scopes.delete(scope);
        if (this.scopes.size === 0) {
            clearTimeout(this.lingerTimer);
            this.lingerTimer = setTimeout(() => this.scopes.size === 0 && this.stop(), LINGER_MS);
        }
    }

    stop() {
        clearTimeout(this.sweepTimer);
        clearTimeout(this.lingerTimer);
        this.running = false;
        this.live = false;
        clearTimeout(this.retryTimer);
        this.controller?.abort();
        // Only this stream's own entry: a newer stream for the same URL may have taken its place.
        if (streams.get(this.url) === this) {
            streams.delete(this.url);
        }
    }

    async connect() {
        if (!this.running) {
            return;
        }
        this.live = false;
        this.refusedLast = false;
        let planned = false;
        this.controller = new AbortController();
        try {
            const response = await this.open(this.controller.signal);
            if (!response.ok || !response.body) {
                throw new Error('stream answered ' + response.status);
            }
            planned = (await this.read(response.body.getReader())) === 'planned';
        } catch (error) {
            if (!this.running) {
                return;
            }
        }
        this.live = false;
        if (!this.running) {
            return;
        }
        this.prune();
        for (const scope of this.scopes) {
            scope.dropped(planned);
        }
        // The relay's short `retry:` is for its planned end only; a failure waits the normal time, longer after a refusal.
        const wait = this.refusedLast ? RETRY_MS * 5 : planned ? this.retryMs : RETRY_MS;
        this.retryMs = RETRY_MS;
        this.retryTimer = setTimeout(() => {
            // Reconnect only for a page still on screen.
            this.prune();
            if (this.scopes.size === 0) {
                this.stop();

                return;
            }
            this.connect();
        }, wait);
    }

    async read(reader) {
        const decoder = new TextDecoder();
        let buffer = '';
        for (;;) {
            const { value, done } = await reader.read();
            if (done) {
                return;
            }
            buffer += decoder.decode(value, { stream: true }).replace(/\r\n?/g, '\n');
            let end;
            while ((end = buffer.indexOf('\n\n')) !== -1) {
                const frame = buffer.slice(0, end);
                buffer = buffer.slice(end + 2);
                const outcome = this.handle(frame);
                if (outcome === 'end' || outcome === 'planned') {
                    this.controller.abort();
                    return outcome;
                }
            }
        }
    }

    handle(frame) {
        let name = 'message';
        const data = [];
        for (const line of frame.split('\n')) {
            if (line === '' || line.startsWith(':')) {
                continue;
            }
            const at = line.indexOf(':');
            const field = at === -1 ? line : line.slice(0, at);
            let value = at === -1 ? '' : line.slice(at + 1);
            if (value.startsWith(' ')) {
                value = value.slice(1);
            }
            if (field === 'event') {
                name = value;
            } else if (field === 'data') {
                data.push(value);
            } else if (field === 'retry' && /^\d+$/.test(value)) {
                this.retryMs = Number(value);
            }
        }
        let payload = null;
        try {
            payload = data.length ? JSON.parse(data.join('\n')) : null;
        } catch (error) {
            return null;
        }

        if (name === 'hello') {
            this.live = true;
            for (const scope of this.scopes) {
                scope.begin();
            }
        } else if (name === 'change' || name === 'run_event') {
            for (const scope of this.scopes) {
                scope.receive(name, payload);
            }
        } else if (name === 'director_error') {
            this.refusedLast = true;
            for (const scope of this.scopes) {
                scope.refused(payload?.detail ?? 'The Director refused the stream.');
            }
            return 'end';
        } else if (name === 'revoked' || name === 'resync') {
            return 'end';
        } else if (name === 'relay_end') {
            return 'planned';
        }
        return null;
    }
}

/**
 * A page's (or the sidebar's) scope on the shared stream: its status for the dot, and the reconnect order for its
 * own $wire.
 */
export function directorStream(url) {
    // Outside the component's data: Alpine wraps data in a reactive proxy, and a proxied stream is not the object the
    // shared list holds, so the stream did not know itself and closed its own new connection on every page change.
    let stream = null;

    return {
        status: 'connecting',
        phase: 'waiting',
        held: { changes: [], runEvents: [] },
        queue: { changes: [], runEvents: [] },
        flushTimer: null,
        flushing: false,
        stopped: false,
        quiet: false,
        /** The element this scope belongs to; once it has left the page, the stream forgets the scope. */
        root: null,
        round: 0,

        init() {
            this.root = this.$el;
            stream = SharedStream.for(url);
            stream.join(this);
        },

        destroy() {
            this.stopped = true;
            clearTimeout(this.flushTimer);
            stream?.leave(this);
        },

        /** The stream said hello (or was already live when this scope joined): hold, refetch, apply. */
        begin() {
            this.phase = 'holding';
            this.round++;
            this.held = { changes: [], runEvents: [] };
            if (!this.quiet) {
                this.status = 'syncing';
            }
            this.resync();
        },

        receive(name, payload) {
            const target = this.phase === 'holding' ? this.held : this.phase === 'live' ? this.queue : null;
            if (target) {
                (name === 'change' ? target.changes : target.runEvents).push(payload);
                if (this.phase === 'live') {
                    this.scheduleFlush();
                }
            }
        },

        dropped(planned) {
            this.phase = 'waiting';
            this.quiet = planned && this.status === 'live';
            if (!this.quiet) {
                this.status = this.status === 'refused' ? 'refused' : 'reconnecting';
            }
        },

        refused(detail) {
            this.status = 'refused';
            this.call('streamRefused', detail);
        },

        async resync() {
            const round = this.round;
            await this.call('refetch');
            // A drop or a newer hello while refetching: that round's own resync takes over.
            if (this.stopped || this.phase !== 'holding' || round !== this.round) {
                return;
            }
            // Live at once: what was held goes ahead of anything that arrives from now on, in one queue, so an event
            // that lands while the held ones are being applied is not lost. Revisions keep stale ones from winning.
            const held = this.held;
            this.held = { changes: [], runEvents: [] };
            this.queue = {
                changes: [...held.changes, ...this.queue.changes],
                runEvents: [...held.runEvents, ...this.queue.runEvents],
            };
            this.phase = 'live';
            this.status = 'live';
            this.quiet = false;
            this.flush();
        },

        scheduleFlush() {
            if (this.flushTimer || this.flushing) {
                return;
            }
            this.flushTimer = setTimeout(() => {
                this.flushTimer = null;
                this.flush();
            }, FLUSH_MS);
        },

        /**
         * Send what is queued, one call at a time: while a call is out, new events wait for it, so a slow page is not
         * sent a pile of overlapping calls.
         */
        async flush() {
            if (this.flushing) {
                return;
            }
            clearTimeout(this.flushTimer);
            this.flushTimer = null;
            const batch = this.queue;
            this.queue = { changes: [], runEvents: [] };
            if (batch.changes.length || batch.runEvents.length) {
                this.flushing = true;
                try {
                    await this.call('applyStream', batch.changes, batch.runEvents);
                } finally {
                    this.flushing = false;
                }
            }
            if (this.queue.changes.length || this.queue.runEvents.length) {
                this.scheduleFlush();
            }
        },

        /** A call on this scope's component, skipped once the scope has left (its page is gone). */
        async call(method, ...args) {
            if (this.stopped) {
                return;
            }
            try {
                await this.$wire[method](...args);
            } catch (error) {
                // A component removed mid-call (navigation) has nothing left to update.
            }
        },
    };
}

if (typeof document !== 'undefined') {
    document.addEventListener('alpine:init', () => {
        window.Alpine.data('directorStream', directorStream);
    });
}
