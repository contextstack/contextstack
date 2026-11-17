// npm test (node --test resources/js/)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { closingFence, composer, fenceLine, toMarkdown } from './composer.js';

const doc = (...content) => ({ type: 'doc', content });
const p = (...content) => ({ type: 'paragraph', content });
const t = (text, ...marks) => ({ type: 'text', text, marks: marks.map((type) => ({ type })) });

test('inline code, bold and line breaks come back as Markdown', () => {
    assert.equal(toMarkdown(doc(p(t('what about '), t('Cursor', 'code'), t(' and '), t('this', 'bold'), { type: 'hardBreak' }, t('next line')))),
        'what about `Cursor` and **this**\nnext line');
});

test('a code block keeps its text exactly, with its language', () => {
    const code = { type: 'codeBlock', attrs: { language: 'php' }, content: [t("echo 'hi';\n    indented")] };
    assert.equal(toMarkdown(doc(p(t('Here:')), code, p(t('Thanks.')))), "Here:\n\n```php\necho 'hi';\n    indented\n```\n\nThanks.");
});

test('pasted lines stay one line break apart; lists and links survive', () => {
    const list = { type: 'bulletList', content: [{ type: 'listItem', content: [p(t('one'))] }, { type: 'listItem', content: [p(t('two'))] }] };
    const link = { type: 'text', text: 'docs', marks: [{ type: 'link', attrs: { href: 'https://laravel.com' } }] };
    assert.equal(toMarkdown(doc(p(t('a')), p(t('b')), list, p(link))), 'a\nb\n\n- one\n- two\n\n[docs](https://laravel.com)');
});

test('an empty editor sends nothing', () => {
    assert.equal(toMarkdown(doc({ type: 'paragraph' })), '');
});

/*
 * The fence keys. ProseMirror is not loaded here: these stand-ins give fenceLine and closingFence the few parts of
 * an editor state they read. A paragraph's children are strings (text) and BR (Shift+Enter); the cursor sits at the
 * paragraph's end, which starts at position 1 in the document.
 */
const BR = Symbol('hardBreak');

function paragraphState(...children) {
    const cells = [];
    const nodes = children.map((child) => {
        if (child === BR) {
            cells.push('');

            return { type: { name: 'hardBreak' }, size: 1 };
        }
        cells.push(...child);

        return { type: { name: 'text' }, size: child.length };
    });
    const start = 1;
    const end = start + cells.length;
    const paragraph = {
        type: { name: 'paragraph' },
        content: { size: cells.length },
        forEach(callback) {
            let offset = 0;
            for (const node of nodes) {
                callback(node, offset);
                offset += node.size;
            }
        },
    };

    return {
        selection: { empty: true, $from: { parent: paragraph, parentOffset: cells.length, start: () => start, end: () => end } },
        doc: { textBetween: (from, to) => cells.slice(from - start, to - start).join('') },
    };
}

function codeState(text) {
    const start = 1;
    const end = start + text.length;

    return {
        selection: {
            empty: true,
            $from: { parent: { type: { name: 'codeBlock' }, content: { size: text.length }, textContent: text }, parentOffset: text.length, end: () => end, after: () => end + 1 },
        },
    };
}

test('a fence alone on the first line opens a block, with its language', () => {
    assert.deepEqual(fenceLine(paragraphState('```php')), { from: 1, to: 7, split: false, language: 'php' });
    assert.deepEqual(fenceLine(paragraphState('```')), { from: 1, to: 4, split: false, language: '' });
});

test('a fence on a later line (after Shift+Enter) opens a block below the text, taking the blank lines with it', () => {
    // "first line", two line breaks, then ```: delete from the first break, so no blank line stays above the block.
    assert.deepEqual(fenceLine(paragraphState('first line', BR, BR, '```')), { from: 11, to: 16, split: true, language: '' });
    assert.deepEqual(fenceLine(paragraphState('use ', BR, '```js ')), { from: 5, to: 12, split: true, language: 'js' });
});

test('only a line that is just the fence counts', () => {
    assert.equal(fenceLine(paragraphState('say ```')), null);
    assert.equal(fenceLine(paragraphState('first', BR, '``` more')), null);
    assert.equal(fenceLine(paragraphState('``')), null);
    const notAtEnd = paragraphState('```');
    notAtEnd.selection.$from.parentOffset = 1;
    assert.equal(fenceLine(notAtEnd), null);
});

test('a closing fence inside a code block is found with the line break before it', () => {
    // "nope\n```": delete "\n```" (positions 5..9); the block then ends at 5, so the next block starts at 6.
    assert.deepEqual(closingFence(codeState('nope\n```')), { from: 5, to: 9, after: 6 });
    assert.equal(closingFence(codeState('nope\n``` x')), null);
    assert.equal(closingFence(codeState('echo "```"')), null);
});

/** A composer whose editor records what it was asked to do, and whose send() only counts. */
function keyed(state, inCode) {
    const calls = [];
    const chain = new Proxy({}, {
        get: (_, name) => (name === 'run' ? () => calls.push('run') : (...args) => (calls.push(name), args, chain)),
    });
    const box = composer();
    box.$refs = { editor: { editor: { state, isActive: (type) => type === 'codeBlock' && inCode, chain: () => chain } } };
    box.sent = 0;
    box.send = () => box.sent++;
    const press = (key, modifiers = {}) => {
        const event = { key, isComposing: false, shiftKey: false, metaKey: false, ctrlKey: false, altKey: false, ...modifiers, prevented: false };
        event.preventDefault = () => (event.prevented = true);
        event.stopPropagation = () => {};
        box.keydown(event);

        return event;
    };

    return { box, calls, press };
}

test('Shift+Enter, Enter or a space on a fence line make a code block; none of them send', () => {
    for (const [key, modifiers] of [['Enter', { shiftKey: true }], ['Enter', {}], [' ', {}]]) {
        const { box, calls, press } = keyed(paragraphState('first', BR, '```'), false);
        assert.equal(press(key, modifiers).prevented, true);
        assert.ok(calls.includes('setCodeBlock'), `${key} makes a code block`);
        assert.equal(box.sent, 0);
    }
});

test('in plain text Enter sends and Shift+Enter is left to the editor (a line break)', () => {
    const enter = keyed(paragraphState('hello'), false);
    enter.press('Enter');
    assert.equal(enter.box.sent, 1);
    const shift = keyed(paragraphState('hello'), false);
    assert.equal(shift.press('Enter', { shiftKey: true }).prevented, false);
    assert.equal(shift.box.sent, 0);
});

test('in a code block Shift+Enter is a new line (not leaving the block), Cmd+Enter sends, ``` then Enter closes it', () => {
    const shift = keyed(codeState('nope'), true);
    shift.press('Enter', { shiftKey: true });
    assert.deepEqual(shift.calls, ['insertContent', 'scrollIntoView', 'run']);
    assert.equal(shift.box.sent, 0);

    const send = keyed(codeState('nope'), true);
    send.press('Enter', { metaKey: true });
    assert.equal(send.box.sent, 1);

    const close = keyed(codeState('nope\n```'), true);
    close.press('Enter', { shiftKey: true });
    assert.deepEqual(close.calls, ['command', 'insertContentAt', 'focus', 'run']);
    assert.equal(close.box.sent, 0);
});

test('the Enter that confirms an IME candidate (keyCode 229, as Safari sends it) does not send', () => {
    const { box, press } = keyed(paragraphState('こんにちは'), false);
    assert.equal(press('Enter', { keyCode: 229 }).prevented, false);
    assert.equal(box.sent, 0);
});

/** A composer in a form, sending through the real send(); `disabled` is the form's submit button state. */
function inForm(disabled) {
    const box = composer({ form: true });
    const form = { submitted: 0, requestSubmit() { this.submitted++; }, querySelector: () => ({ disabled }) };
    box.$root = { closest: () => form };
    box.$wire = {};
    box.$refs = { editor: { editor: { getJSON: () => doc(p(t('hello'))) } } };

    return { box, form };
}

test('in a form, Enter submits once the button is enabled, and not while it is disabled (sending, or nothing can start)', async () => {
    const ready = inForm(false);
    await ready.box.send();
    assert.equal(ready.form.submitted, 1);
    assert.equal(ready.box.$wire.message, 'hello');

    const busy = inForm(true);
    await busy.box.send();
    assert.equal(busy.form.submitted, 0);
});

test('the box is read-only while a message sends, so nothing typed meanwhile is cleared with it', async () => {
    const box = composer();
    const editable = [];
    let cleared = false;
    let finish;
    box.$wire = { post: () => new Promise((resolve) => (finish = resolve)) };
    box.$refs = { editor: { editor: {
        getJSON: () => doc(p(t('hello'))),
        setEditable: (value) => editable.push(value),
        commands: { clearContent: () => (cleared = true), focus: () => {} },
    } } };

    const sending = box.send();
    assert.deepEqual(editable, [false]);
    box.$wire.message = '';
    finish();
    await sending;
    assert.deepEqual(editable, [false, true]);
    assert.equal(cleared, true);
});
