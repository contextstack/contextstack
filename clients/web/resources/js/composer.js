/**
 * The message composer: Flux's rich-text editor (Tiptap) dressed as a chat box, sending Markdown.
 *
 * As you type, Markdown turns into what it means: `code` into inline code, **bold**, ``` then Enter (or a space)
 * into a code block you can paste into. What is sent is Markdown again (toMarkdown below), so the model and the
 * thread see exactly what a plain text box would have sent.
 *
 * A fence works on any line, not just the first: text, Shift+Enter, ```php, then Enter, Shift+Enter or a space.
 * Inside the block, ``` alone on a line then Enter closes it, as typing a fence by hand would.
 *
 * Keys: Enter sends; Shift+Enter is a new line; inside a code block Enter and Shift+Enter are a new line and Cmd/Ctrl+Enter sends.
 */

/**
 * Markdown for a Tiptap document (editor.getJSON()). Covers what the editor can make from typing and pasting:
 * paragraphs, line breaks, headings, lists, quotes, rules, code blocks, and the bold, italic, strike, code and link
 * marks. Paragraphs that follow each other are joined by one line break (they come from pasted lines); other blocks
 * by a blank line.
 *
 * @param {object} doc
 * @returns {string}
 */
export function toMarkdown(doc) {
    return blocks(doc?.content ?? []).trim();
}

function blocks(nodes) {
    let out = '';
    let previous = null;
    for (const node of nodes) {
        const text = block(node);
        if (previous !== null) {
            out += previous === 'paragraph' && node.type === 'paragraph' ? '\n' : '\n\n';
        }
        out += text;
        previous = node.type;
    }

    return out;
}

function block(node) {
    switch (node.type) {
        case 'paragraph':
            return inline(node.content ?? []);
        case 'heading':
            return '#'.repeat(node.attrs?.level ?? 1) + ' ' + inline(node.content ?? []);
        case 'codeBlock': {
            const code = (node.content ?? []).map((child) => child.text ?? '').join('');
            const fence = code.includes('```') ? '````' : '```';

            return fence + (node.attrs?.language ?? '') + '\n' + code + '\n' + fence;
        }
        case 'blockquote':
            return blocks(node.content ?? []).split('\n').map((line) => (line === '' ? '>' : '> ' + line)).join('\n');
        case 'bulletList':
            return list(node.content ?? [], () => '- ');
        case 'orderedList': {
            const start = node.attrs?.start ?? 1;

            return list(node.content ?? [], (index) => `${start + index}. `);
        }
        case 'horizontalRule':
            return '---';
        default:
            return node.content ? blocks(node.content) : (node.text ?? '');
    }
}

function list(items, marker) {
    return items
        .map((item, index) => {
            const prefix = marker(index);
            const body = blocks(item.content ?? []);

            return prefix + body.split('\n').join('\n' + ' '.repeat(prefix.length));
        })
        .join('\n');
}

function inline(nodes) {
    return nodes
        .map((node) => {
            if (node.type === 'hardBreak') {
                return '\n';
            }
            if (node.type !== 'text') {
                return node.content ? inline(node.content) : '';
            }

            return marked(node.text ?? '', node.marks ?? []);
        })
        .join('');
}

function marked(text, marks) {
    const has = (type) => marks.find((mark) => mark.type === type);
    if (has('code')) {
        const ticks = text.includes('`') ? '``' : '`';
        text = ticks + (ticks === '``' ? ' ' + text + ' ' : text) + ticks;
    } else {
        if (has('bold')) {
            text = '**' + text + '**';
        }
        if (has('italic')) {
            text = '*' + text + '*';
        }
        if (has('strike')) {
            text = '~~' + text + '~~';
        }
    }
    const link = has('link');

    return link?.attrs?.href ? '[' + text + '](' + link.attrs.href + ')' : text;
}

/**
 * The fence on the line the cursor ends, if that line is only ``` and a language: the text after the paragraph's
 * last line break (Shift+Enter), with the cursor at the end of it. Gives the range to delete (with the line breaks
 * just before it, so no blank line is left above the block; then the paragraph needs splitting when text remains)
 * and the language.
 *
 * @returns {{from: number, to: number, split: boolean, language: string}|null}
 */
export function fenceLine(state) {
    const { $from, empty } = state.selection;
    const paragraph = $from.parent;
    if (!empty || paragraph.type.name !== 'paragraph' || $from.parentOffset !== paragraph.content.size) {
        return null;
    }
    const children = [];
    paragraph.forEach((child, offset) => children.push({ name: child.type.name, offset }));
    let last = -1;
    children.forEach((child, index) => {
        if (child.name === 'hardBreak') {
            last = index;
        }
    });
    const start = $from.start();
    const lineFrom = last === -1 ? start : start + children[last].offset + 1;
    const match = FENCE.exec(state.doc.textBetween(lineFrom, $from.end()));
    if (!match) {
        return null;
    }
    if (last === -1) {
        return { from: start, to: $from.end(), split: false, language: match[1] };
    }
    let first = last;
    while (first > 0 && children[first - 1].name === 'hardBreak') {
        first--;
    }

    return { from: start + children[first].offset, to: $from.end(), split: first > 0, language: match[1] };
}

/**
 * A closing fence typed inside a code block: the block's last line is only ```, with the cursor at its end. Gives
 * the range to delete (the line and the line break before it), and where the block then ends, for the paragraph
 * that follows it.
 *
 * @returns {{from: number, to: number, after: number}|null}
 */
export function closingFence(state) {
    const { $from, empty } = state.selection;
    const block = $from.parent;
    if (!empty || block.type.name !== 'codeBlock' || $from.parentOffset !== block.content.size) {
        return null;
    }
    const lines = block.textContent.split('\n');
    const line = lines[lines.length - 1];
    if (!/^```[ \t\u00a0]*$/.test(line)) {
        return null;
    }

    const from = $from.end() - line.length - (lines.length > 1 ? 1 : 0);

    return { from, to: $from.end(), after: $from.after() - ($from.end() - from) };
}

const FENCE = /^```([A-Za-z0-9_+-]*)[ \t\u00a0]*$/;

/**
 * The composer's Alpine state. `$refs.editor` is the <ui-editor>; its `.editor` is the Tiptap editor.
 *
 * In a form (`composer({ form: true })`, the new-conversation page) the editor keeps `$wire.message` current as
 * you type, so the form's own button sends it too, and Enter submits the form instead of calling post().
 */
export function composer(options = {}) {
    const inForm = options.form === true;

    return {
        empty: true,
        sending: false,

        init() {
            const element = this.$refs.editor;
            const ready = () => {
                const editor = element.editor;
                this.empty = editor.isEmpty;
                editor.on('update', () => {
                    this.empty = editor.isEmpty;
                    if (inForm) {
                        this.$wire.message = toMarkdown(editor.getJSON());
                    }
                });
            };
            element.editor ? ready() : element.addEventListener('flux:editor:ready', ready, { once: true });
        },

        /** A click anywhere in the box, not only on the text's lines, puts the cursor in it (at the end). */
        focusBox(event) {
            const editor = this.$refs.editor?.editor;
            if (!editor || event.target.closest?.('[contenteditable="true"], button, a, select, input')) {
                return;
            }
            editor.commands.focus('end');
        },

        /** Keydown on the editor, in the capture phase, so it runs before the editor's own keys. */
        keydown(event) {
            const editor = this.$refs.editor.editor;
            const space = event.key === ' ' && !event.metaKey && !event.ctrlKey && !event.altKey;
            // keyCode 229: Safari sends the Enter that confirms an IME candidate with isComposing already false.
            if (!editor || (event.key !== 'Enter' && !space) || event.isComposing || event.keyCode === 229) {
                return;
            }
            const stop = () => {
                event.preventDefault();
                event.stopPropagation();
            };
            const inCode = editor.isActive('codeBlock');

            if (inCode && event.key === 'Enter' && !event.metaKey && !event.ctrlKey) {
                // ``` on its own line inside the block, then Enter: close the block and carry on below it.
                const close = closingFence(editor.state);
                if (close) {
                    stop();
                    editor
                        .chain()
                        .command(({ tr }) => (tr.delete(close.from, close.to), true))
                        .insertContentAt(close.after, { type: 'paragraph' })
                        .focus(close.after + 1)
                        .run();

                    return;
                }
                if (event.shiftKey) {
                    // Tiptap's Shift+Enter leaves the block; here it is a new line, as everywhere else.
                    stop();
                    editor.chain().insertContent('\n').scrollIntoView().run();

                    return;
                }
            }

            if (!inCode && !event.metaKey && !event.ctrlKey) {
                // ``` (or ```php) alone on the current line, then Enter, Shift+Enter or a space: make it a code block.
                // Tiptap's own ``` + space rule only fires at the start of a block; this covers any line.
                const fence = fenceLine(editor.state);
                if (fence) {
                    stop();
                    editor
                        .chain()
                        .command(({ tr }) => (tr.delete(fence.from, fence.to), true))
                        .command(({ commands }) => (fence.split ? commands.splitBlock() : true))
                        .setCodeBlock(fence.language ? { language: fence.language } : {})
                        .run();

                    return;
                }
            }

            const send = !space && (inCode ? event.metaKey || event.ctrlKey : !event.shiftKey);
            if (send) {
                stop();
                this.send();
            }
        },

        async send() {
            const editor = this.$refs.editor.editor;
            const markdown = toMarkdown(editor.getJSON());
            if (inForm) {
                // The form's own button is disabled while it sends (and when nothing can start): Enter respects it,
                // so a second Enter does not start a second conversation.
                const form = this.$root.closest('form');
                if (!form || form.querySelector('button[type="submit"]')?.disabled) {
                    return;
                }
                this.$wire.message = markdown;
                form.requestSubmit();

                return;
            }
            if (markdown === '' || this.sending) {
                return;
            }
            this.sending = true;
            this.$wire.message = markdown;
            // Read-only while it sends: text typed meanwhile would be cleared with the sent message.
            editor.setEditable(false);
            try {
                await this.$wire.post();
            } finally {
                this.sending = false;
                editor.setEditable(true);
            }
            // The page empties `message` once the Director took it; on a refusal it keeps it, and so does the box.
            if (this.$wire.message === '') {
                editor.commands.clearContent(true);
            }
            editor.commands.focus();
        },
    };
}

if (typeof document !== 'undefined') {
    document.addEventListener('alpine:init', () => {
        window.Alpine.data('composer', composer);
    });
}
