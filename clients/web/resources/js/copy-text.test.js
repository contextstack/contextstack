import { test } from 'node:test';
import assert from 'node:assert/strict';
import { copyText } from './copy-text.js';

/** A document whose copy command succeeds or not, recording what was selected. */
function fakeDocument(copies) {
    const doc = { copied: null, body: { appendChild: () => {} } };
    doc.createElement = () => ({ style: {}, setAttribute: () => {}, select() { doc.copied = this.value; }, remove: () => {} });
    doc.execCommand = () => copies;

    return doc;
}

test('uses the clipboard when the page has one', async () => {
    let written = null;
    assert.equal(await copyText('hi', fakeDocument(false), { writeText: async (text) => (written = text) }), true);
    assert.equal(written, 'hi');
});

test('falls back to the copy command on a plain-http page, and says when that fails too', async () => {
    const doc = fakeDocument(true);
    assert.equal(await copyText('hi', doc, undefined), true);
    assert.equal(doc.copied, 'hi');
    assert.equal(await copyText('hi', fakeDocument(false), { writeText: async () => { throw new Error('denied'); } }), false);
});
