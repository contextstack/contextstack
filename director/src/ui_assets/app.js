'use strict';

// User messages stay plain text; assistant replies are rendered as Markdown.
const content = document.querySelector('#content');
const notice = document.querySelector('#notice');
let csrf = '';
let generation = 0;
const pending = new Map();
let liveSource = null;
let livePoll = null;
let liveConversation = null;
let refreshTimer = null;

const PENDING_STATES = new Set(['running', 'queued', 'summarising']);

const DRAFT_DEBOUNCE_MS = 300;
const draftSaveTimers = new Map();

function draftStorageKey(conversationId) {
  return `cs-draft:${conversationId}`;
}

function readStoredDraft(conversationId) {
  try {
    const value = sessionStorage.getItem(draftStorageKey(conversationId));
    return value === null ? '' : value;
  } catch {
    return '';
  }
}

function writeStoredDraft(conversationId, text) {
  try {
    const key = draftStorageKey(conversationId);
    if (text) sessionStorage.setItem(key, text);
    else sessionStorage.removeItem(key);
  } catch {
    // sessionStorage may be unavailable; drafts are best-effort.
  }
}

function clearStoredDraft(conversationId) {
  try {
    sessionStorage.removeItem(draftStorageKey(conversationId));
  } catch {
    // sessionStorage may be unavailable; drafts are best-effort.
  }
}

function cancelDraftSave(conversationId) {
  const timer = draftSaveTimers.get(conversationId);
  if (!timer) return;
  clearTimeout(timer);
  draftSaveTimers.delete(conversationId);
}

function scheduleDraftSave(conversationId, input) {
  cancelDraftSave(conversationId);
  draftSaveTimers.set(conversationId, setTimeout(() => {
    draftSaveTimers.delete(conversationId);
    writeStoredDraft(conversationId, input.value);
  }, DRAFT_DEBOUNCE_MS));
}


function turnsPending(turns) {
  return (turns || []).some((turn) => PENDING_STATES.has(turn.state) && !(turn.reply ?? turn.detail));
}

function stopLiveUpdates() {
  if (liveSource) {
    liveSource.close();
    liveSource = null;
  }
  if (livePoll) {
    clearInterval(livePoll);
    livePoll = null;
  }
  liveConversation = null;
  if (refreshTimer) {
    clearTimeout(refreshTimer);
    refreshTimer = null;
  }
}

function scheduleConversationRefresh() {
  if (refreshTimer) return;
  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    if (!liveConversation) return;
    const hash = `conversation=${encodeURIComponent(liveConversation)}`;
    if (location.hash.slice(1) === hash) void render();
  }, 200);
}

function startLiveUpdates(conversationId) {
  if (liveConversation === conversationId) return;
  stopLiveUpdates();
  liveConversation = conversationId;
  // Reconcile even a healthy stream: completion may precede its subscription.
  livePoll = setInterval(scheduleConversationRefresh, 10000);
  if (typeof EventSource !== 'undefined') {
    const url = `/ui/api/events?conversation=${encodeURIComponent(conversationId)}`;
    liveSource = new EventSource(url, { withCredentials: true });
    liveSource.addEventListener('change', scheduleConversationRefresh);
    liveSource.addEventListener('run_event', scheduleConversationRefresh);
    liveSource.addEventListener('resync', scheduleConversationRefresh);
    liveSource.onerror = scheduleConversationRefresh;
  }
}

function appendReply(card, turn) {
  const reply = turn.reply ?? turn.detail ?? ({
    running: 'Waiting for the reply…', queued: 'Queued.', summarising: 'Preparing a summary…',
  }[turn.state] || '');
  if (!reply) return;
  const box = element('div', null, 'message reply');
  if (turn.reply && typeof globalThis.renderMarkdown === 'function') {
    box.classList.add('md');
    box.append(globalThis.renderMarkdown(reply));
  } else {
    box.textContent = reply;
    if (PENDING_STATES.has(turn.state) && !turn.reply) box.classList.add('pending');
  }
  card.append(box);
}


function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined && text !== null) node.textContent = String(text);
  if (className) node.className = className;
  return node;
}

async function request(path, options = {}) {
  const response = await fetch(path, { credentials: 'same-origin', ...options });
  if (response.status === 401 || response.status === 403) {
    stopLiveUpdates();
    const message = 'Session expired. Run cs-director ui-link and open the new link.';
    notice.textContent = message;
    throw new Error(message);
  }
  const answer = await response.json();
  if (!response.ok) {
    throw new Error(answer.detail || `Request failed (${response.status}).`);
  }
  return answer;
}

function badge(text) { return element('span', text, 'badge'); }
function attention(conversation, parent) {
  if (!conversation.attention) return;
  const box = element('div', null, 'attention');
  const source = conversation.attention.source_kind === 'lead'
    ? `Needs you — from Hosted lead ${conversation.conversation_id}` : 'Needs you';
  box.append(element('strong', source), element('p', conversation.attention.question, 'message'));
  parent.append(box);
}

function empty(parent, items, message) {
  if (!items.length) parent.append(element('p', message, 'meta'));
}

async function statusPage(parent) {
  const [nodes, jobs] = await Promise.all([
    request('/ui/api/nodes'), request('/ui/api/jobs'),
  ]);
  parent.append(element('h1', 'Status'), element('h2', 'Nodes'));
  empty(parent, nodes.nodes, 'No nodes registered yet.');
  for (const node of nodes.nodes) {
    const card = element('article');
    const state = node.revoked_at ? 'Revoked' : node.stale ? 'Stale' : 'Online';
    card.append(element('h3', node.name), badge(state));
    card.append(element('p', `Last seen: ${node.last_seen_at || 'Never'} · Open jobs: ${node.open_jobs}`, 'meta'));
    parent.append(card);
  }
  parent.append(element('h2', 'Recent jobs'));
  const recent = jobs.jobs.slice(0, 25);
  empty(parent, recent, 'No jobs yet.');
  for (const job of recent) {
    const card = element('article');
    card.append(element('h3', job.job_id), badge(job.state));
    card.append(element('p', `${job.node || ''} · ${job.created_at || ''}`, 'meta'));
    if (job.conversation_id) {
      const link = element('a', 'Open conversation');
      link.href = `#conversation=${encodeURIComponent(job.conversation_id)}`;
      card.append(link);
    }
    parent.append(card);
  }
}

async function conversationsPage(parent) {
  const answer = await request('/ui/api/conversations');
  parent.append(element('h1', 'Conversations'));
  const [options, nodes] = await Promise.all([
    request('/ui/api/models'), request('/ui/api/nodes'),
  ]);
  const form = element('form', null, 'panel');
  const modelLabel = element('label', 'Model');
  const model = element('select');
  model.id = 'new-model';
  modelLabel.htmlFor = model.id;
  for (const choice of options.models) {
    if (choice.internal) continue;
    const option = element('option', choice.label);
    option.value = choice.name;
    option.disabled = !choice.available;
    option.selected = choice.name === options.default_model;
    model.append(option);
  }
  const label = element('label', 'First message (optional)');
  const message = element('textarea');
  message.id = 'first-message';
  message.maxLength = 200000;
  label.htmlFor = message.id;
  const button = element('button', 'New conversation');
  const feedback = element('p', '', 'meta');
  const node = nodes.nodes.find(n => !n.revoked_at);
  button.disabled = !node;
  if (!node) feedback.textContent = 'Register a node with cs init first.';
  form.append(modelLabel, model, label, message, button, feedback);
  let draft;
  form.addEventListener('submit', async event => {
    event.preventDefault();
    if (!draft || draft.message !== message.value || draft.model !== model.value) {
      draft = { message: message.value, model: model.value, key: crypto.randomUUID() };
    }
    button.disabled = true;
    try {
      const created = await request('/ui/api/conversations', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': csrf, 'Idempotency-Key': draft.key },
        body: JSON.stringify({ node: node.name, model: draft.model }),
      });
      const id = created.conversation.conversation_id;
      if (draft.message.trim()) await request(`/ui/api/conversations/${encodeURIComponent(id)}/turns`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': csrf, 'Idempotency-Key': draft.key + '-turn' },
        body: JSON.stringify({ message: draft.message }),
      });
      location.hash = 'conversation=' + encodeURIComponent(id);
    } catch (error) {
      feedback.textContent = error.message + ' You can retry safely.';
    } finally { button.disabled = false; }
  });
  parent.append(form);
  empty(parent, answer.conversations, 'No conversations yet. Start one above.');
  for (const conversation of answer.conversations) {
    const card = element('article');
    const heading = element('h2');
    const link = element('a', conversation.title || conversation.conversation_id);
    link.href = `#conversation=${encodeURIComponent(conversation.conversation_id)}`;
    heading.append(link);
    card.append(heading, badge(conversation.state));
    card.append(element('p', conversation.node, 'meta'));
    attention(conversation, card);
    parent.append(card);
  }
}

async function conversationPage(parent, id, current) {
  const path = `/ui/api/conversations/${encodeURIComponent(id)}`;
  startLiveUpdates(id);
  const answer = await request(`${path}/thread`);
  if (current !== generation) return;
  if (!turnsPending(answer.turns)) stopLiveUpdates();
  const thread = element('section');
  thread.id = 'thread';
  parent.append(thread);
  const conversation = answer.conversation;
  thread.append(element('h1', conversation.title || id), badge(conversation.state));
  attention(conversation, thread);
  thread.append(element('h2', 'Turns'));
  empty(thread, answer.turns, 'No turns yet. Send a message below.');
  for (const turn of answer.turns) {
    const card = element('article');
    card.append(element('p', `#${turn.seq} · ${turn.posted_by_name || turn.posted_by} · ${turn.posted_at}`, 'meta'));
    card.append(badge(turn.state), element('p', turn.message, 'message'));
    appendReply(card, turn);
    thread.append(card);
  }
  const form = element('form', null, 'panel');
  const label = element('label', 'Message');
  form.dataset.conversation = id;
  label.htmlFor = 'message';
  const input = element('textarea');
  input.id = 'message';
  input.name = 'message';
  input.required = true;
  input.maxLength = 200000;
  const stored = pending.get(id)?.message;
  input.value = stored !== undefined ? stored : readStoredDraft(id);
  input.addEventListener('input', () => scheduleDraftSave(id, input));
  const submit = element('button', 'Send message');
  submit.type = 'submit';
  const feedback = element('p', '', 'meta');
  feedback.setAttribute('role', 'status');
  form.append(label, input, submit, feedback);
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (!input.value.trim()) return;
    // Retain the key and message after an ambiguous network failure so a retry cannot duplicate a turn.
    let draft = pending.get(id);
    if (!draft || draft.message !== input.value) {
      draft = { message: input.value, key: crypto.randomUUID() };
      pending.set(id, draft);
    }
    submit.disabled = true;
    input.readOnly = true;
    feedback.textContent = 'Sending…';
    try {
      await request(`${path}/turns`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': csrf, 'Idempotency-Key': draft.key },
        body: JSON.stringify({ message: draft.message }),
      });
      pending.delete(id);
      cancelDraftSave(id);
      clearStoredDraft(id);
      input.value = '';
      feedback.textContent = 'Message sent.';
      if (location.hash === `#conversation=${encodeURIComponent(id)}`) await render();
    } catch (error) {
      feedback.textContent = `${error.message} Your message is retained; you can retry.`;
    } finally {
      submit.disabled = false;
      input.readOnly = false;
    }
  });
  parent.append(form);
}

async function render() {
  if (!csrf) return;
  const current = ++generation;
  notice.textContent = 'Loading…';
  const page = document.createDocumentFragment();
  const hash = location.hash.slice(1) || 'status';
  document.querySelectorAll('[data-page]').forEach((link) => {
    if (link.dataset.page === (hash.startsWith('conversation') ? 'conversations' : 'status')) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  });
  try {
    if (hash.startsWith('conversation=')) await conversationPage(page, decodeURIComponent(hash.slice(13)), current);
    else if (hash === 'conversations') await conversationsPage(page);
    else await statusPage(page);
    if (current !== generation) return;
    const oldComposer = content.querySelector('form[data-conversation]');
    const newComposer = page.querySelector('form[data-conversation]');
    const sameConversation = oldComposer && newComposer
      && oldComposer.dataset.conversation === newComposer.dataset.conversation;
    if (sameConversation) {
      content.querySelector('#thread').replaceWith(page.querySelector('#thread'));
    } else {
      content.replaceChildren(page);
    }
    notice.textContent = '';
    if (hash.startsWith('conversation=')) {
      const input = content.querySelector('#message');
      if (!sameConversation && input && !input.value) input.focus();
    } else {
      stopLiveUpdates();
    }
  } catch (error) {
    if (current !== generation) return;
    stopLiveUpdates();
    content.replaceChildren();
    notice.textContent = `${error.message} To connect, run cs-director ui-link and open the link it prints.`;
  }
}

async function start() {
  // The fragment never reaches HTTP logs, and is removed from history before exchange.
  const params = new URLSearchParams(location.hash.slice(1));
  const code = params.get('code');
  if (code !== null) {
    const conversation = params.get('conversation');
    history.replaceState(null, '', conversation ? `/ui/#conversation=${encodeURIComponent(conversation)}` : '/ui/#status');
  }
  try {
    if (code !== null) await request('/ui/pair', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ code }),
    });
    const session = await request('/ui/session');
    csrf = session.csrf;
    const logout = document.querySelector('#logout');
    logout.disabled = false;
    logout.addEventListener('click', async () => {
      logout.disabled = true;
      try {
        await request('/ui/logout', { method: 'POST', headers: { 'X-CSRF-Token': csrf } });
        csrf = '';
        ++generation;
        pending.clear();
        stopLiveUpdates();
        content.replaceChildren();
        notice.textContent = 'Logged out. Run cs-director ui-link to connect again.';
      } catch (error) {
        notice.textContent = error.message;
        logout.disabled = false;
      }
    });
    window.addEventListener('hashchange', render);
    document.querySelector('#refresh').addEventListener('click', render);
    await render();
  } catch (error) {
    notice.textContent = `${error.message} To connect, run cs-director ui-link and open the link it prints.`;
  }
}

start();
