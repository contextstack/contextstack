'use strict';
const assert = require('assert/strict');
const fs = require('fs');
const vm = require('vm');

// Keep node identity, focus and selection so refreshes exercise the live composer.
class Node {
  constructor(tag = '') {
    this.tagName = tag;
    this.childNodes = [];
    this.dataset = {};
    this.listeners = {};
    this.value = '';
    this.classList = { add() {} };
  }
  append(...nodes) {
    for (const node of nodes) {
      if (node.parent) node.parent.childNodes.splice(node.parent.childNodes.indexOf(node), 1);
      this.childNodes.push(node);
      node.parent = this;
    }
  }
  replaceChildren(...nodes) { this.childNodes = []; this.append(...nodes); }
  replaceWith(node) {
    const i = this.parent.childNodes.indexOf(this);
    this.parent.childNodes[i] = node;
    node.parent = this.parent;
  }
  setAttribute() {}
  removeAttribute() {}
  addEventListener(name, fn) { this.listeners[name] = fn; }
  focus() { document.activeElement = this; }
  querySelector(selector) {
    for (const child of this.childNodes) {
      if ((selector[0] === '#' && child.id === selector.slice(1))
        || (selector === 'form[data-conversation]' && child.tagName === 'form' && child.dataset.conversation)) return child;
      const found = child.querySelector(selector);
      if (found) return found;
    }
    return null;
  }
}
const content = new Node();
const notice = new Node();
const document = {
  createElement: tag => new Node(tag),
  createDocumentFragment: () => new Node(),
  querySelector: selector => selector === '#content' ? content : notice,
  querySelectorAll: () => [],
};
const order = [];
const sources = [];
const intervals = new Map();
const timeouts = new Map();
let nextTimer = 1;
let turns = [{ seq: 1, state: 'running', message: 'hello' }];
let status = 200;
let turnStatus = 200;
let fetches = 0;
let uuid = 0;
const draftStore = new Map();
const context = {
  document, location: { hash: '#conversation=c1' }, console,
  crypto: { randomUUID: () => `00000000-0000-4000-8000-${String(++uuid).padStart(12, '0')}` },
  sessionStorage: {
    getItem: (key) => (draftStore.has(key) ? draftStore.get(key) : null),
    setItem: (key, value) => { draftStore.set(key, String(value)); },
    removeItem: (key) => { draftStore.delete(key); },
  },
  setInterval: (fn, ms) => { const id = nextTimer++; intervals.set(id, { fn, ms }); return id; },
  clearInterval: id => intervals.delete(id),
  setTimeout: fn => { const id = nextTimer++; timeouts.set(id, fn); return id; },
  clearTimeout: id => timeouts.delete(id),
  EventSource: class {
    constructor() { order.push('stream'); this.listeners = {}; sources.push(this); }
    addEventListener(name, fn) { this.listeners[name] = fn; }
    close() { this.closed = true; }
  },
  fetch: async (url, options = {}) => {
    order.push('fetch');
    fetches++;
    const path = String(url);
    if (options.method === 'POST' && path.includes('/turns')) {
      const ok = turnStatus === 200;
      return {
        ok,
        status: turnStatus,
        json: async () => (ok ? { turn: { seq: 2 } } : { detail: 'server error' }),
      };
    }
    const ok = status === 200;
    return { ok, status, json: async () => ({ conversation: { conversation_id: 'c1' }, turns }) };
  },
};
vm.createContext(context);
const source = fs.readFileSync(require('path').join(__dirname, '../src/ui_assets/app.js'), 'utf8');
vm.runInContext(source.replace(/start\(\);\s*$/, "csrf = 'fixture';"), context);
const run = expression => vm.runInContext(expression, context);
const flush = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };
async function fireRefresh() {
  for (const [id, fn] of [...timeouts]) { timeouts.delete(id); fn(); }
  await flush();
}

(async () => {
  await run('render()');
  assert.deepEqual(order.slice(0, 2), ['stream', 'fetch'], 'subscribe before thread fetch');
  const input = content.querySelector('#message');
  input.value = 'a follow-up < &';
  input.selectionStart = 2;
  input.selectionEnd = 8;
  input.focus();
  sources[0].listeners.change();
  await fireRefresh();
  assert.equal(content.querySelector('#message'), input, 'event refresh keeps composer node');
  assert.equal(input.value, 'a follow-up < &');
  assert.equal(input.selectionStart, 2);
  assert.equal(input.selectionEnd, 8);
  assert.equal(document.activeElement, input);
  assert.equal(sources.length, 1, 'refresh keeps subscription');

  // No SSE event arrives for completion: the reconciliation timer must fetch it.
  assert.equal([...intervals.values()][0].ms, 10000);
  turns = [{ seq: 1, state: 'done', message: 'hello', reply: 'finished' }];
  const before = fetches;
  [...intervals.values()][0].fn();
  await fireRefresh();
  assert.equal(fetches, before + 1, 'poll reconciles missed completion');
  assert.equal(intervals.size, 0, 'completion stops polling');
  assert(sources[0].closed);
  assert.equal(content.querySelector('#message'), input, 'poll keeps draft');

  for (const failure of [401, 403, 'render']) {
    status = 200;
    turnStatus = 200;
    turns = [{ seq: 2, state: 'running', message: 'again' }];
    await run('render()');
    if (failure === 'render') {
      turns = [{ state: 'done', reply: 'bad render' }];
      context.renderMarkdown = () => { throw new Error('renderer failed'); };
    } else status = failure;
    await run('render()');
    assert.equal(intervals.size, 0, `${failure} stops polling`);
    assert.equal(timeouts.size, 0, `${failure} cancels refresh`);
    assert(sources.at(-1).closed, `${failure} closes stream`);
    assert.match(notice.textContent, /connect.*cs-director ui-link/, `${failure} shows login instructions`);
  }

  delete context.renderMarkdown;
  status = 200;
  turnStatus = 200;
  await run('render()');
  const reloadInput = content.querySelector('#message');
  reloadInput.value = 'persisted draft';
  reloadInput.listeners.input();
  await fireRefresh();
  assert.equal(draftStore.get('cs-draft:c1'), 'persisted draft', 'draft saved to sessionStorage');
  await run('pending.clear()');
  await run('render()');
  assert.equal(content.querySelector('#message').value, 'persisted draft', 'draft restored after reload');

  context.location.hash = '#status';
  await run('render()');
  draftStore.set('cs-draft:c1', 'send me');
  context.location.hash = '#conversation=c1';
  await run('render()');
  const sendInput = content.querySelector('#message');
  const sendForm = content.querySelector('form[data-conversation]');
  assert.equal(sendInput.value, 'send me');
  await sendForm.listeners.submit({ preventDefault() {} });
  await flush();
  await fireRefresh();
  assert.equal(draftStore.has('cs-draft:c1'), false, 'draft cleared after successful send');
  assert.equal(content.querySelector('#message').value, '', 'composer cleared after successful send');

  status = 200;
  turnStatus = 500;
  context.location.hash = '#status';
  await run('render()');
  draftStore.set('cs-draft:c1', 'retry me');
  context.location.hash = '#conversation=c1';
  await run('render()');
  const retryInput = content.querySelector('#message');
  const retryForm = content.querySelector('form[data-conversation]');
  assert.equal(retryInput.value, 'retry me');
  await retryForm.listeners.submit({ preventDefault() {} });
  await flush();
  assert.equal(draftStore.get('cs-draft:c1'), 'retry me', 'draft kept after failed send');
  assert.equal(retryInput.value, 'retry me', 'input kept after failed send');

  context.sessionStorage = {
    getItem() { throw new Error('storage blocked'); },
    setItem() { throw new Error('storage blocked'); },
    removeItem() { throw new Error('storage blocked'); },
  };
  status = 200;
  turnStatus = 200;
  await run('render()');
  const brokenInput = content.querySelector('#message');
  const brokenForm = content.querySelector('form[data-conversation]');
  brokenInput.value = 'type without storage';
  brokenInput.listeners.input();
  await fireRefresh();
  brokenInput.value = 'send without storage';
  await brokenForm.listeners.submit({ preventDefault() {} });
  await flush();
  await fireRefresh();
  assert.equal(brokenInput.value, '', 'send succeeds when sessionStorage throws');

  console.log('ui_app_test: ok');
})().catch(error => { console.error(error); process.exitCode = 1; });
