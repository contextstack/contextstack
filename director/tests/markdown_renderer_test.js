'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');

function makeEl(tag) {
  const node = {
    tagName: tag.toUpperCase(),
    className: '',
    childNodes: [],
    attrs: {},
    _text: '',
    appendChild(child) {
      this.childNodes.push(child);
    },
    set textContent(v) {
      this._text = String(v);
      this.childNodes = [{ nodeType: 3, textContent: this._text }];
    },
    get textContent() {
      if (this._text) return this._text;
      return this.childNodes.map((c) => c.textContent ?? '').join('');
    },
    setAttribute(k, v) {
      this.attrs[k] = v;
    },
    getAttribute(k) {
      return this.attrs[k] ?? null;
    },
  };
  return node;
}

// Browser serialization escapes text nodes and attribute values exactly once.
function escapeText(value) {
  return String(value).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function htmlOf(node) {
  if (!node) return '';
  if (node.nodeType === 3) return escapeText(node.textContent);
  const inner = node.childNodes.map(htmlOf).join('');
  if (!node.tagName || node.tagName === 'FRAGMENT') return inner;
  const attrs = Object.entries(node.attrs)
    .map(([k, v]) => `${k}="${escapeText(v).replace(/"/g, '&quot;')}"`)
    .join(' ');
  const open = attrs.length ? `<${node.tagName.toLowerCase()} ${attrs}>` : `<${node.tagName.toLowerCase()}>`;
  return `${open}${inner}</${node.tagName.toLowerCase()}>`;
}

const dom = {
  createElement: makeEl,
  createTextNode: (text) => ({ nodeType: 3, textContent: String(text) }),
  createDocumentFragment: () => makeEl('fragment'),
};

const src = fs.readFileSync(path.join(__dirname, '../src/ui_assets/markdown.js'), 'utf8');
const context = { document: dom, globalThis: {} };
vm.createContext(context);
vm.runInContext(src, context);

const { renderMarkdown } = context.globalThis;

function assert(cond, msg) {
  if (!cond) {
    console.error('FAIL:', msg);
    process.exit(1);
  }
}

const script = renderMarkdown('<script>alert(1)</script>');
const scriptHtml = htmlOf(script);
assert(!scriptHtml.includes('<script>'), 'raw script tag must not appear');
assert(scriptHtml.includes('&lt;script&gt;'), 'script must be escaped');

const jsLink = renderMarkdown('[click](javascript:alert(1))');
const jsHtml = htmlOf(jsLink);
assert(!jsHtml.includes('javascript:'), 'javascript: links must be dropped');
assert(jsHtml.includes('click'), 'link label should remain as text');

for (const [source, want] of [
  ['*italic*', '<em>italic</em>'],
  ['_italic_', '<em>italic</em>'],
  ['**bold**', '<strong>bold</strong>'],
  ['**outer *inner* outer**', '<strong>outer <em>inner</em> outer</strong>'],
  ['a *b* c **d**', '<em>b</em>'],
]) {
  let html = '';
  try {
    html = htmlOf(renderMarkdown(source));
  } catch (error) {
    assert(false, 'emphasis threw for ' + JSON.stringify(source) + ': ' + error.message);
  }
  assert(html.includes(want), 'emphasis ' + JSON.stringify(source) + ' -> ' + html);
}

const fence = renderMarkdown('```\nconst x = 1;\n```');
const fenceHtml = htmlOf(fence);
assert(fenceHtml.includes('<pre>'), 'fenced block should use pre');
assert(fenceHtml.includes('const x = 1;'), 'fenced code content preserved');

const image = '<img src=x onerror=alert(1)>';
const imageFence = renderMarkdown('```html\n' + image + '\n```');
assert(imageFence.textContent === image, 'fenced markup stays literal text without pre-escaping');
assert(htmlOf(imageFence).includes('&lt;img src=x onerror=alert(1)&gt;'), 'serialization escapes fenced HTML');
assert(!htmlOf(imageFence).includes('&amp;lt;'), 'fenced HTML is escaped exactly once');

const quote = renderMarkdown('> quote & more');
assert(htmlOf(quote) === '<blockquote><p>quote &amp; more</p></blockquote>', 'blockquote marker is parsed before DOM serialization');

function firstLink(node) {
  if (node.tagName === 'A') return node;
  return (node.childNodes || []).map(firstLink).find(Boolean);
}
const url = 'https://example.test/?a=1&b=2';
const safeLink = renderMarkdown('[query](' + url + ')');
assert(firstLink(safeLink).getAttribute('href') === url, 'ampersand in URL remains unchanged in the DOM');
assert(htmlOf(safeLink).includes('?a=1&amp;b=2'), 'URL ampersand is escaped once on serialization');
for (const [source, expected] of [
  [' HTTPS://example.test/ ', 'HTTPS://example.test/'],
  ['mailto:person@example.test', 'mailto:person@example.test'],
  ['https&#58;//example.test/?a=1&amp;b=2', url],
  ['https&colon;//example.test/', 'https://example.test/'],
]) {
  const link = firstLink(renderMarkdown('[safe](' + source + ')'));
  assert(link && link.getAttribute('href') === expected, 'decoded safe scheme: ' + source);
}
for (const scheme of [
  'javascript:alert', 'JaVaScRiPt:alert', 'data:text/html,evil',
  'jav&#x61;script:alert', 'javascript&colon;alert', '&#106;avascript:alert',
  ' &Tab;javascript:alert ', 'data&#58;text/html,evil',
]) {
  const result = renderMarkdown('[unsafe](' + scheme + ')');
  assert(!firstLink(result), 'unsafe scheme refused: ' + scheme);
  assert(result.textContent === 'unsafe', 'refused URL retains label text');
}

console.log('markdown_renderer_test: ok');
