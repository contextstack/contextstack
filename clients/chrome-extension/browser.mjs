import { claimedBrowserAction, executeFormAction } from './form-action.mjs';

const HOST = 'com.contextstack.browser';
const DOCUMENT_TYPES = ['main_frame', 'sub_frame'];
const REQUEST_TIMEOUT = 40_000;
const RETRY_DELAY = 2_000;
const MAX_RETRY_DELAY = 60_000;
const SCREENSHOT_GRANT_TIMEOUT = 120_000;
const SCREENSHOT_ORIGIN = '<all_urls>';
const TEXT_LIMIT = 1_000_000;
const TASKS_ORIGIN = 'https://tasks.google.com';
const POPUP_PERMISSION = 'contentSettings';

export function httpsOrigin(value) {
  try {
    const url = new URL(value);
    if (url.protocol === 'https:' && !url.username && !url.password) return url.origin;
  } catch {
    // An invalid or unparseable URL has no allowed origin.
  }
  return null;
}

function frameOrigin(url, parentOrigin, mainFrame) {
  if (mainFrame) return httpsOrigin(url);
  if (url === 'about:blank' || url === 'about:srcdoc') return parentOrigin;
  return httpsOrigin(url.startsWith('blob:') ? url.slice(5) : url);
}

export function navigationRules(tabId, origins, capture = false) {
  const scope = { tabIds: [tabId], resourceTypes: DOCUMENT_TYPES };
  const declared = Array.from(origins);
  const cspId = declared.length + 2;
  return [
    { id: 1, priority: 1, action: { type: 'block' }, condition: scope },
    ...declared.map((origin, i) => ({
      id: i + 2,
      priority: 2,
      action: { type: 'allow' },
      condition: {
        ...scope,
        regexFilter: '^' + origin.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + '/',
      },
    })),
    {
      id: cspId,
      priority: 3,
      action: { type: 'modifyHeaders', responseHeaders: [{
        header: 'content-security-policy', operation: 'append',
        value: 'sandbox allow-scripts allow-same-origin allow-forms',
      }] },
      condition: scope,
    },
    ...(capture ? [{ id: cspId + 1, priority: 4, action: { type: 'block' },
      condition: { tabIds: [tabId], resourceTypes: DOCUMENT_TYPES, requestMethods: ['post'] } }] : []),
  ];
}

function readPage(origins, limit) {
  if (!origins.includes(location.origin) || location.protocol !== 'https:') return null;
  if (document.querySelector('form input[autocomplete="current-password"]') ||
      (location.origin === 'https://tasks.google.com' &&
      (document.querySelector('input[type="password"], input[type="email"]') ||
       document.querySelector('form[action^="https://accounts.google.com/"]')))) {
    return { final_url: location.href, sign_in_required: true };
  }
  return {
    final_url: location.href,
    status: null,
    title: document.title.slice(0, 4096),
    text: (document.body?.innerText ?? '').slice(0, limit),
  };
}

export function start(chrome) {
  const dnr = chrome.declarativeNetRequest;
  let active;
  let missing = [];
  let screenshotTimer;
  let cleanup = clearOldRules();
  cleanup.catch(() => {});

  async function clearOldRules() {
    const rules = await dnr.getSessionRules();
    const tabIds = new Set(rules.flatMap(rule => rule.condition.tabIds ?? []));
    await Promise.all([...tabIds].map(closeManagedTab));
    if (rules.length) await dnr.updateSessionRules({ removeRuleIds: rules.map(rule => rule.id) });
    if (await chrome.permissions.contains({ permissions: [POPUP_PERMISSION] })) {
      await chrome.contentSettings.popups.clear({ scope: 'regular' });
    }
    await removeScreenshotGrant();
  }

  async function removeScreenshotGrant() {
    clearTimeout(screenshotTimer);
    const scope = { origins: [SCREENSHOT_ORIGIN] };
    if (await chrome.permissions.contains(scope)) await chrome.permissions.remove(scope);
  }

  async function closeManagedTab(tabId) {
    try {
      await chrome.tabs.remove(tabId);
    } catch (error) {
      // A tab that Chrome or the owner already closed needs no further cleanup.
      await chrome.tabs.get(tabId).then(() => { throw error; }, () => {});
    }
  }

  async function captureTab() {
    const window = await chrome.windows.create({ url: 'about:blank', focused: false });
    if (window?.id === undefined) throw new Error('Could not open the capture window');
    try {
      const [tab] = await chrome.tabs.query({ windowId: window.id, active: true });
      if (!tab || window.focused || (await chrome.windows.get(window.id)).focused) {
        throw new Error('Could not create an unfocused capture window');
      }
      return tab;
    } catch (error) {
      await chrome.windows.remove(window.id).catch(() => {});
      throw error;
    }
  }

  function finish(request, terminal) {
    if (active !== request) return;
    active = undefined;
    clearTimeout(request.timer);
    const closeTab = request.tabId === undefined ? Promise.resolve() : closeManagedTab(request.tabId);
    cleanup = Promise.all([request.setup, closeTab]).then(async () => {
      if (request.popupSettings) await chrome.contentSettings.popups.clear({ scope: 'regular' });
      if (request.rules.length) await dnr.updateSessionRules({ removeRuleIds: request.rules });
      if (request.capture) await removeScreenshotGrant();
    });
    // A failed cleanup must prevent reuse of the rule IDs on the next open.
    cleanup.catch(() => {});
    if (terminal) request.port.postMessage({ ...terminal, id: request.id });
  }

  function fail(request, message, error_kind = request.approval ? 'action_refused' : 'unknown') {
    if (request.executing) {
      finish(request, { type: 'action_result', status: null });
      return;
    }
    finish(request, { type: 'error', message, error_kind });
  }

  function report(request, url, mainFrame = false, parentOrigin = null) {
    const event = ++request.event;
    request.pending.add(event);
    const navigation = { type: 'navigation', id: request.id, event, url };
    if (mainFrame) navigation.main_frame = true;
    if (parentOrigin) navigation.parent_origin = parentOrigin;
    request.port.postMessage(navigation);
    if (!request.origins.has(frameOrigin(url, parentOrigin, mainFrame))) {
      fail(request, 'Navigation is outside the declared origins', 'navigation_blocked');
    }
  }

  async function launch(request) {
    if (request.launching || request.pending.size || active !== request) return;
    request.launching = true;
    try {
      await cleanup;
      if (active !== request) return;
      const grants = await Promise.all(request.patterns.map(async pattern =>
        await chrome.permissions.contains({ origins: [pattern] }) ? null : pattern));
      if (!await chrome.permissions.contains({ permissions: [POPUP_PERMISSION] })) grants.push(POPUP_PERMISSION);
      if (active !== request) return;
      missing = [...new Set([...missing, ...grants.filter(Boolean)])];
      if (grants.some(Boolean)) {
        void chrome.action.setBadgeText({ text: '!' }).catch(() => {});
        fail(request, 'Chrome has not granted site access', 'origin_not_permitted');
        return;
      }
      if (!missing.length) await chrome.action.setBadgeText({ text: '' });
      if (active !== request) return;
      request.setup = (async () => {
        if (active !== request) return;
        request.popupSettings = true;
        for (const origin of request.origins) {
          await chrome.contentSettings.popups.set({ primaryPattern: `${origin}/*`, setting: 'block', scope: 'regular' });
        }
        if (active !== request) return;
        const tab = request.capture ? await captureTab()
          : await chrome.tabs.create({ url: 'about:blank', active: false });
        if (active !== request) {
          await chrome.tabs.remove(tab.id).catch(() => {});
          return;
        }
        request.tabId = tab.id;
        request.windowId = tab.windowId;
        const rules = navigationRules(tab.id, request.origins, request.capture);
        await dnr.updateSessionRules({ addRules: rules });
        request.rules = rules.map(rule => rule.id);
        if (active !== request) return;
        const groupId = await chrome.tabs.group({ tabIds: [tab.id] });
        if (active !== request) return;
        await chrome.tabGroups.update(groupId, { title: 'ContextStack' });
        if (active !== request) return;
        request.navigating = true;
        await chrome.tabs.update(tab.id, { url: request.url });
      })().catch(() => fail(request, 'Could not start the managed tab'));
    } catch {
      fail(request, 'Could not check site permissions');
    } finally {
      request.launching = false;
    }
  }

  function open(port, message) {
    if (active) finish(active);
    if (!Number.isSafeInteger(message.id) || message.id < 0) return;
    const origins = new Set(Array.isArray(message.origins) ? message.origins : []);
    if (!origins.size || [...origins].some(origin => httpsOrigin(origin) !== origin || origin === null)
      || typeof message.url !== 'string') {
      port.postMessage({ type: 'error', id: message.id, message: 'Invalid URL or HTTPS origin list' });
      return;
    }
    const capture = message.type === 'capture_action';
    const approval = capture || message.type === 'inspect_action' ? message.approval : null;
    if (message.type === 'inspect_action' &&
        (!claimedBrowserAction(approval) || approval.origin !== httpsOrigin(message.url)
          || approval.origin === TASKS_ORIGIN)) {
      port.postMessage({ type: 'error', id: message.id, message: 'Invalid browser action claim', error_kind: 'action_refused' });
      return;
    }
    if (capture && (!approval || approval.origin !== httpsOrigin(message.url) ||
        approval.origin === TASKS_ORIGIN || typeof approval.target !== 'string')) {
      port.postMessage({ type: 'error', id: message.id, message: 'Invalid browser capture target', error_kind: 'action_refused' });
      return;
    }
    // Chrome match patterns cover every port. The navigation rules still match exact origins.
    const patterns = [...new Set([...origins].map(origin => `https://${new URL(origin).hostname}/*`))];
    const request = {
      id: message.id, url: message.url, port, origins, patterns: capture ? [...patterns, SCREENSHOT_ORIGIN] : patterns,
      approval, capture, event: 0, pending: new Set(),
      frames: new Map(), frameOrigins: new Map(), loading: new Set(), setup: cleanup, rules: [],
    };
    active = request;
    request.timer = setTimeout(() => fail(request, 'Browser request timed out', 'page_timeout'), REQUEST_TIMEOUT);
    report(request, request.url);
  }

  async function complete(request) {
    if (request.reading || request.executing || request.pending.size || request.loading.size || !request.document) return;
    const { url, documentId } = request.document;
    const revision = request.event;
    request.reading = true;
    try {
      if (request.approval) {
        if (httpsOrigin(url) !== request.approval.origin ||
            (!request.capture && !claimedBrowserAction(request.approval))) {
          fail(request, 'Invalid browser action claim for the managed document');
          return;
        }
        const inspect = !request.confirmed;
        if (inspect && request.snapshotDocument) {
          fail(request, 'The managed document changed after form inspection');
          return;
        }
        if (!inspect) {
          request.executing = true;
          clearTimeout(request.timer);
          request.timer = setTimeout(() => fail(request, 'Browser submission timed out'), REQUEST_TIMEOUT);
        }
        const [action] = await chrome.scripting.executeScript({
          target: { tabId: request.tabId, documentIds: [documentId] },
          func: executeFormAction,
          args: [request.approval, inspect, request.capture],
        });
        if (active !== request) return;
        if (revision !== request.event) {
          fail(request, 'The managed document changed during the action');
        } else if (request.capture && action?.result?.ok === true) {
          const isActive = async () => {
            const [tab] = await chrome.tabs.query({ active: true, windowId: request.windowId });
            const window = await chrome.windows.get(request.windowId);
            return tab?.id === request.tabId && !window.focused;
          };
          const activation = request.activation;
          if (!await isActive()) return fail(request, 'The managed tab is no longer visible');
          const image = await chrome.tabs.captureVisibleTab(request.windowId, { format: 'jpeg', quality: 65 });
          if (!await isActive() || revision !== request.event || activation !== request.activation) {
            return fail(request, 'The managed tab changed during capture');
          }
          const [check] = await chrome.scripting.executeScript({
            target: { tabId: request.tabId, documentIds: [documentId] },
            func: executeFormAction,
            args: [request.approval, true, true],
          });
          if (active !== request) return;
          if (revision !== request.event || check?.result?.digest !== action.result.digest) {
            return fail(request, check?.result?.error ?? 'The form changed during capture');
          }
          finish(request, { type: 'proposal_snapshot', ...action.result, image });
        } else if (inspect && action?.result?.ok === true) {
          request.snapshotDocument = request.document;
          request.port.postMessage({ type: 'action_snapshot', id: request.id, digest: action.result.digest });
        } else if (!inspect && action?.result?.ok === true) {
          finish(request, { type: 'action_result', status: action.result.status });
        } else if (!inspect && action?.result?.ok === false) {
          request.executing = false;
          fail(request, action.result.error, 'action_refused_before_send');
        } else {
          fail(request, action?.result?.error ?? 'Browser form submission failed');
        }
        return;
      }
      const [snapshot] = await chrome.scripting.executeScript({
        target: { tabId: request.tabId, documentIds: [documentId] },
        func: readPage,
        args: [[...request.origins], TEXT_LIMIT],
      });
      if (active !== request || revision !== request.event) return;
      const result = snapshot?.result;
      if (!result || result.final_url !== url || !request.origins.has(httpsOrigin(result.final_url))) {
        fail(request, 'The managed document changed before it could be read');
        return;
      }
      if (result.sign_in_required === true) {
        finish(request, { type: 'logged_out' });
      } else {
        finish(request, { type: 'result', ...result });
      }
    } catch {
      if (request.approval) fail(request, 'Browser form submission failed');
      else if (revision === request.event) fail(request, 'Could not read the managed document');
    } finally {
      request.reading = false;
      if (active === request && revision !== request.event) void complete(request);
    }
  }

  function observe(details, force = false) {
    const request = active;
    if (!request?.navigating || details.tabId !== request.tabId) return;
    const { frameId, url } = details;
    if (force || request.frames.get(frameId) !== url) {
      const inherited = frameId !== 0 && (url === 'about:blank' || url === 'about:srcdoc')
        ? request.frameOrigins.get(details.parentFrameId) : null;
      request.frameOrigins.set(frameId, frameOrigin(url, inherited, frameId === 0));
      request.frames.set(frameId, url);
      report(request, url, frameId === 0 || details.type === 'main_frame', inherited);
    }
    return active === request ? request : undefined;
  }

  chrome.webNavigation.onBeforeNavigate.addListener(details => {
    if (active?.navigating && details.tabId === active.tabId && details.frameId === 0) {
      active.frames.clear();
      active.frameOrigins.clear();
    }
    const request = observe(details, true);
    if (!request) return;
    if (details.frameId === 0) {
      request.document = undefined;
      request.loading.clear();
    }
    request.loading.add(details.frameId);
  });
  chrome.webNavigation.onCommitted.addListener(details => {
    const request = observe(details);
    request?.loading.add(details.frameId);
  });
  chrome.webNavigation.onCompleted.addListener(details => {
    const request = observe(details);
    if (!request) return;
    request.loading.delete(details.frameId);
    if (details.frameId === 0) request.document = details;
    void complete(request);
  });
  for (const event of [chrome.webNavigation.onHistoryStateUpdated, chrome.webNavigation.onReferenceFragmentUpdated]) {
    event.addListener(details => {
      const request = observe(details, true);
      if (!request) return;
      if (details.frameId === 0 && request.document?.documentId === details.documentId) request.document = details;
      void complete(request);
    });
  }
  chrome.webNavigation.onErrorOccurred.addListener(details => {
    const request = observe(details);
    if (request && details.frameId === 0) {
      const kind = details.error === 'net::ERR_BLOCKED_BY_CLIENT' ? 'navigation_blocked'
        : ['net::ERR_TIMED_OUT', 'net::ERR_CONNECTION_TIMED_OUT'].includes(details.error)
          ? 'page_timeout' : 'unknown';
      fail(request, 'Main document navigation failed', kind);
    }
    if (request && details.frameId !== 0) {
      request.loading.delete(details.frameId);
      void complete(request);
    }
  });
  chrome.webRequest.onBeforeRedirect.addListener(details => {
    const request = observe(details);
    if (request) observe({ ...details, url: details.redirectUrl }, true);
  }, { urls: ['<all_urls>'], types: DOCUMENT_TYPES });
  dnr.onRuleMatchedDebug.addListener(({ rule, request: details }) => {
    if (rule.rulesetId !== dnr.SESSION_RULESET_ID) return;
    const request = observe(details);
    if (request && rule.ruleId === 1) fail(request, 'Document navigation blocked', 'navigation_blocked');
  });
  chrome.tabs.onRemoved.addListener(tabId => {
    if (active?.tabId === tabId) fail(active, 'Managed tab closed');
  });
  chrome.webNavigation.onCreatedNavigationTarget.addListener(({ sourceTabId, tabId, url }) => {
    if (active?.tabId !== sourceTabId) return;
    const request = active;
    void chrome.tabs.remove(tabId).catch(() => {});
    report(request, url);
    fail(request, 'The managed page opened a new tab or window');
  });
  chrome.webNavigation.onTabReplaced.addListener(({ replacedTabId, tabId }) => {
    if (active?.tabId !== replacedTabId) return;
    void chrome.tabs.remove(tabId).catch(() => {});
    fail(active, 'The managed tab was replaced');
  });

  chrome.tabs.onActivated?.addListener(({ windowId }) => {
    if (active?.capture && active.windowId === windowId) active.activation = (active.activation ?? 0) + 1;
  });
  chrome.runtime.onMessage.addListener((message, sender, respond) => {
    if (message?.type === 'pending_permissions'
      && sender.url === chrome.runtime.getURL('permissions.html')) respond(missing);
  });
  chrome.permissions.onAdded.addListener(({ origins } = {}) => {
    if (origins?.includes(SCREENSHOT_ORIGIN)) {
      clearTimeout(screenshotTimer);
      screenshotTimer = setTimeout(() => void removeScreenshotGrant().catch(() => {}), SCREENSHOT_GRANT_TIMEOUT);
    }
    void Promise.all(missing.map(async pattern =>
      [pattern, await chrome.permissions.contains(pattern === POPUP_PERMISSION
        ? { permissions: [pattern] } : { origins: [pattern] })]))
      .then(grants => {
        missing = missing.filter(pattern => !grants.some(([granted, ok]) => ok && granted === pattern));
        if (!missing.length) return chrome.action.setBadgeText({ text: '' });
      }).catch(() => {});
  });
  chrome.permissions.onRemoved.addListener(({ origins, permissions }) => {
    if (active?.navigating && (origins?.length || permissions?.includes(POPUP_PERMISSION))) {
      fail(active, 'Site permissions changed', 'origin_not_permitted');
    }
  });

  let retryDelay = RETRY_DELAY;
  function connect() {
    const port = chrome.runtime.connectNative(HOST);
    let connected = true;
    let timer;
    const stable = setTimeout(() => { retryDelay = RETRY_DELAY; }, 30_000);
    async function reportStatus() {
      try {
        const { origins = [] } = await chrome.permissions.getAll();
        if (connected) port.postMessage({ type: 'status', id: 0, origins });
      } catch {
        // A disconnected native host will get a fresh snapshot on reconnection.
      }
      if (connected) timer = setTimeout(reportStatus, 30_000);
    }
    if (chrome.permissions.getAll) void reportStatus();
    port.onMessage.addListener(message => {
      retryDelay = RETRY_DELAY;
      if (['open', 'inspect_action', 'capture_action'].includes(message?.type)) open(port, message);
      if (message?.type === 'cancel_action' && active?.port === port && active.id === message.id
        && active.approval && !active.confirmed) finish(active);
      if (message?.type === 'execute_confirmed' && active?.port === port && active.id === message.id
        && active.approval && !active.capture && active.snapshotDocument === active.document && active.document
        && !active.confirmed) {
        active.confirmed = true;
        void complete(active);
      }
      if (message?.type !== 'navigation_decision' || !active || active.id !== message.id
        || active.port !== port || typeof message.allow !== 'boolean' || !active.pending.delete(message.event)) return;
      if (!message.allow) fail(active, 'Navigation denied by the native host', 'navigation_blocked');
      else if (active.navigating) void complete(active);
      else void launch(active);
    });
    port.onDisconnect.addListener(() => {
      connected = false;
      clearTimeout(timer);
      clearTimeout(stable);
      // Reading lastError suppresses Chrome's missing-host diagnostic during retries.
      void chrome.runtime.lastError;
      if (active?.port === port) finish(active);
      setTimeout(connect, retryDelay);
      retryDelay = Math.min(retryDelay * 2, MAX_RETRY_DELAY);
    });
  }

  connect();
}
