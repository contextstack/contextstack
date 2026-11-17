import { test } from 'node:test';
import assert from 'node:assert/strict';
import { directorStream } from '../../resources/js/director-stream.js';

const frame = (name, data) => `event: ${name}\ndata: ${JSON.stringify(data)}`;
const change = (revision) => frame('change', { kind: 'job', id: 'j1', revision, record: { revision } });

function stream() {
    const calls = [];
    let release;
    const s = directorStream('/stream');
    s.$wire = {
        refetch: () => { calls.push(['refetch']); return new Promise((resolve) => { release = resolve; }); },
        applyStream: async (changes, runEvents) => { calls.push(['apply', changes.map((c) => c.revision), runEvents.map((e) => e.seq)]); },
        streamRefused: (detail) => calls.push(['refused', detail]),
    };
    return { s, calls, release: () => release() };
}

test('events before hello are dropped; after hello they are held until the refetch, then applied in order', async () => {
    const { s, calls, release } = stream();
    s.handle(change(1));
    s.handle(frame('hello', { revision: 1 }));
    s.handle(change(2));
    s.handle(frame('run_event', { run_id: 'r', seq: 1 }));
    assert.deepEqual(calls, [['refetch']], 'nothing is applied while the refetch is out');
    release();
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.deepEqual(calls, [['refetch'], ['apply', [2], [1]]]);
    assert.equal(s.status, 'live');
    s.destroy();
});

test('live events after the refetch are batched and applied', async () => {
    const { s, calls, release } = stream();
    s.handle(frame('hello', { revision: 1 }));
    release();
    await new Promise((resolve) => setTimeout(resolve, 0));
    s.handle(change(3));
    s.handle(change(4));
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.deepEqual(calls.at(-1), ['apply', [3, 4], []]);
    s.destroy();
});

test('revoked, resync and a refusal end the stream so the caller starts again at step 1', () => {
    const { s, calls } = stream();
    assert.equal(s.handle(frame('revoked', {})), 'end');
    assert.equal(s.handle(frame('resync', {})), 'end');
    assert.equal(s.handle(frame('director_error', { detail: 'revoked credential' })), 'end');
    assert.deepEqual(calls, [['refused', 'revoked credential']]);
    s.destroy();
});

test('a relay_end frame is a planned end: its retry hint is kept and a hello after it does not flash syncing', async () => {
    const { s, release } = stream();
    assert.equal(s.handle('retry: 250\nevent: relay_end\ndata: {"reason":"time"}'), 'planned');
    assert.equal(s.retryMs, 250);
    s.status = 'live';
    s.quiet = true;
    s.handle(frame('hello', { revision: 1 }));
    assert.equal(s.status, 'live', 'still live while it refetches');
    release();
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(s.status, 'live');
    assert.equal(s.quiet, false);
    s.destroy();
});

test('the relay ending on time reconnects quietly after its short retry; a drop still shows reconnecting', async () => {
    const realFetch = globalThis.fetch;
    const bodies = [];
    let opened = 0;
    globalThis.fetch = async (_url, { signal }) => {
        opened += 1;
        const text = bodies.shift();
        if (text === undefined) {
            return new Promise((_resolve, reject) => signal.addEventListener('abort', () => reject(new Error('aborted'))));
        }
        return new Response(new Blob([text]).stream(), { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
    };
    try {
        const { s } = stream();
        s.status = 'live';
        bodies.push('retry: 250\nevent: relay_end\ndata: {"reason":"time"}\n\n');
        await s.connect();
        assert.equal(s.status, 'live', 'a planned end does not show reconnecting');
        await new Promise((resolve) => setTimeout(resolve, 400));
        assert.equal(opened, 2, 'reconnected after the 250 ms hint');
        s.destroy();

        const dropped = stream().s;
        dropped.status = 'live';
        bodies.push('event: hello\ndata: {"revision": 1}\n\n');
        await dropped.connect();
        assert.equal(dropped.status, 'reconnecting', 'an unplanned end shows reconnecting');
        dropped.destroy();
    } finally {
        globalThis.fetch = realFetch;
    }
});
