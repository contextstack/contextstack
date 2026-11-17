// npm test (node --test resources/js/)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SharedStream, directorStream } from './director-stream.js';

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

/** A stream whose frames the test pushes, counting how often it was opened. */
function fakeServer() {
    const server = { opened: 0, push: null, close: null };
    server.open = () => {
        server.opened++;
        const body = new ReadableStream({
            start(controller) {
                server.push = (frame) => controller.enqueue(new TextEncoder().encode(frame + '\n\n'));
                server.close = () => controller.close();
            },
        });

        return Promise.resolve({ ok: true, body });
    };

    return server;
}

/** A directorStream() scope whose $wire records the calls. */
function scope() {
    const component = directorStream('unused');
    component.calls = [];
    component.$wire = new Proxy({}, { get: (_, method) => async (...args) => component.calls.push([method, ...args]) });

    return component;
}

test('two scopes share one connection, and each refetches on hello then gets the events', async () => {
    const server = fakeServer();
    const stream = new SharedStream('/stream', server.open);
    const sidebar = scope();
    const page = scope();
    stream.join(sidebar);
    stream.join(page);
    await tick();
    assert.equal(server.opened, 1);

    server.push('event: hello\ndata: {}');
    await tick();
    await tick();
    assert.deepEqual(sidebar.calls, [['refetch']]);
    assert.deepEqual(page.calls, [['refetch']]);
    assert.equal(page.status, 'live');

    server.push('event: change\ndata: {"kind":"job","revision":2}');
    await tick();
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.deepEqual(page.calls.at(-1), ['applyStream', [{ kind: 'job', revision: 2 }], []]);
    assert.deepEqual(sidebar.calls.at(-1), ['applyStream', [{ kind: 'job', revision: 2 }], []]);
    stream.stop();
});

test('a scope joining a live stream (the next page after wire:navigate) refetches at once, without a new connection', async () => {
    const server = fakeServer();
    const stream = new SharedStream('/stream', server.open);
    const sidebar = scope();
    stream.join(sidebar);
    await tick();
    server.push('event: hello\ndata: {}');
    await tick();

    const oldPage = scope();
    stream.join(oldPage);
    oldPage.destroy();
    const nextPage = scope();
    stream.join(nextPage);
    await tick();
    assert.equal(server.opened, 1);
    assert.deepEqual(nextPage.calls, [['refetch']]);
    assert.equal(nextPage.status, 'live');
    stream.stop();
});

test('a dropped stream marks every scope reconnecting; a refused one tells each page why', async () => {
    const server = fakeServer();
    const stream = new SharedStream('/stream', server.open);
    const page = scope();
    stream.join(page);
    await tick();
    server.push('event: director_error\ndata: {"detail":"no credential"}');
    await tick();
    await tick();
    assert.equal(page.status, 'refused');
    assert.deepEqual(page.calls, [['streamRefused', 'no credential']]);
    stream.stop();

    const server2 = fakeServer();
    const stream2 = new SharedStream('/stream', server2.open);
    const other = scope();
    stream2.join(other);
    await tick();
    server2.push('event: hello\ndata: {}');
    await tick();
    await tick();
    server2.close();
    await tick();
    await tick();
    assert.equal(other.status, 'reconnecting');
    stream2.stop();
});

test('a change that arrives while held changes are being applied is applied too, not dropped', async () => {
    const server = fakeServer();
    const stream = new SharedStream('/stream', server.open);
    const page = scope();
    let finishRefetch;
    let finishApply;
    page.$wire = new Proxy({}, {
        get: (_, method) => (...args) => {
            page.calls.push([method, ...args]);
            if (method === 'refetch') {
                return new Promise((resolve) => (finishRefetch = resolve));
            }
            if (method === 'applyStream' && finishApply === undefined) {
                return new Promise((resolve) => (finishApply = resolve));
            }

            return Promise.resolve();
        },
    });
    stream.join(page);
    await tick();
    server.push('event: hello\ndata: {}');
    await tick();
    server.push('event: change\ndata: {"kind":"turn","revision":1}');
    await tick();
    finishRefetch();
    await tick();
    assert.deepEqual(page.calls.at(-1), ['applyStream', [{ kind: 'turn', revision: 1 }], []]);

    server.push('event: change\ndata: {"kind":"turn","revision":2}');
    await tick();
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(page.calls.filter(([method]) => method === 'applyStream').length, 1, 'waits for the call in flight');

    finishApply();
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.deepEqual(page.calls.at(-1), ['applyStream', [{ kind: 'turn', revision: 2 }], []]);
    stream.stop();
});

test('opening another conversation closes the last one\'s stream at once instead of letting it linger', async () => {
    const first = fakeServer();
    const second = fakeServer();
    const a = SharedStream.for('/stream?conversation=a', first.open);
    const pageA = scope();
    a.join(pageA);
    await tick();
    a.leave(pageA);
    assert.equal(a.running, true, 'lingers while nothing else has joined');

    const b = SharedStream.for('/stream?conversation=b', second.open);
    const pageB = scope();
    b.join(pageB);
    await tick();
    assert.equal(a.running, false);
    assert.equal(b.running, true);
    b.stop();
});

test('a page replaced without destroy() stops holding its stream: joining elsewhere or reconnecting drops it', async () => {
    const first = fakeServer();
    const second = fakeServer();
    const a = SharedStream.for('/stream?conversation=leaked', first.open);
    const leaked = scope();
    leaked.root = { isConnected: true };
    a.join(leaked);
    await tick();
    leaked.root.isConnected = false;

    const b = SharedStream.for('/stream?conversation=next', second.open);
    b.join(scope());
    await tick();
    assert.equal(a.running, false, 'the replaced page\'s stream closed when the next page joined');

    const kept = scope();
    kept.root = { isConnected: true };
    b.join(kept);
    await tick();
    second.push('event: hello\ndata: {}');
    await tick();
    for (const s of [...b.scopes]) {
        if (s !== kept) {
            b.scopes.delete(s);
        }
    }
    kept.root.isConnected = false;
    second.push('retry: 1\nevent: relay_end\ndata: {}');
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.equal(b.running, false, 'no reconnect for a page that is gone');
    assert.equal(second.opened, 1);
});

test('the page being replaced is closed once it has left, even though it was still attached when the next page joined', async () => {
    const first = fakeServer();
    const second = fakeServer();
    const a = SharedStream.for('/stream?conversation=before', first.open);
    const before = scope();
    before.root = { isConnected: true };
    a.join(before);
    await tick();

    const b = SharedStream.for('/stream?conversation=after', second.open);
    b.join(scope());
    await tick();
    assert.equal(a.running, true, 'still attached at the join');

    before.root.isConnected = false;
    await new Promise((resolve) => setTimeout(resolve, 1100));
    assert.equal(a.running, false);
    b.stop();
});

test('a stream reached through a reactive proxy (Alpine data) does not close itself when its page joins', async () => {
    const server = fakeServer();
    const raw = SharedStream.for('/stream?conversation=proxied', server.open);
    const proxied = new Proxy(raw, {});
    proxied.join(scope());
    await tick();
    assert.equal(raw.running, true);
    assert.equal(SharedStream.for('/stream?conversation=proxied'), raw, 'still listed, so the next page can close it');
    assert.equal(server.opened, 1);
    raw.stop();
});
