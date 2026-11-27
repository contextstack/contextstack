const SHA256 = /^[a-f0-9]{64}$/;
const TTL_MS = 300_000;
const CLOCK_SKEW_MS = 30_000;

export function claimedBrowserAction(approval, now = Date.now()) {
  const decision = Date.parse(approval?.decided_at);
  const claim = Date.parse(approval?.claimed_at);
  return approval?.kind === 'browser_action' && approval.status === 'approved'
    && approval.outcome?.status === 'unknown'
    && ['submit', 'post', 'buy'].includes(approval.action)
    && typeof approval.target === 'string' && approval.target.length > 0
    && typeof approval.field_values_digest === 'string' && SHA256.test(approval.field_values_digest)
    && typeof approval.screenshot_sha === 'string' && SHA256.test(approval.screenshot_sha)
    && decision <= claim && claim <= now + CLOCK_SKEW_MS && now - decision < TTL_MS;
}

// Chrome serializes this function into the managed document's isolated world.
export async function executeFormAction({ origin, target, field_values_digest, decided_at }, inspect, capture) {
  const refused = error => ({ ok: false, error });
  if (capture) {
    if (!globalThis.contextstackCapture) {
      globalThis.contextstackCapture = { typed: false };
      addEventListener('keydown', event => {
        if (event.isTrusted) {
          globalThis.contextstackCapture.typed = true;
          event.preventDefault();
          event.stopImmediatePropagation();
        }
      }, true);
    }
    if (globalThis.contextstackCapture.typed) return refused('keyboard input reached the capture tab');
  }
  if (location.origin !== origin || location.protocol !== 'https:') return refused('origin changed');
  let form;
  try {
    const matches = document.querySelectorAll(target);
    if (matches.length === 1 && matches[0] instanceof HTMLFormElement) form = matches[0];
  } catch {
    return refused('invalid form target');
  }
  if (!form) return refused('expected exactly one form matching the target');
  if (['action', 'method', 'enctype'].some(name => typeof form[name] !== 'string')
    || form.elements == null || typeof form.elements[Symbol.iterator] !== 'function'
    || (typeof HTMLFormControlsCollection !== 'undefined' &&
        !(form.elements instanceof HTMLFormControlsCollection))
    || (form.acceptCharset != null && typeof form.acceptCharset !== 'string')) {
    return refused('form control overrides a form property');
  }
  if (form.method !== 'post' || form.enctype !== 'application/x-www-form-urlencoded') {
    return refused('expected one URL-encoded POST form');
  }
  if (form.acceptCharset?.trim() && !/^utf-8$/i.test(form.acceptCharset.trim())) {
    return refused('unsupported form accept-charset; expected UTF-8');
  }
  const endpoint = new URL(form.action);
  if (endpoint.origin !== origin || endpoint.username || endpoint.password) {
    return refused('form destination is outside the approved origin');
  }
  const fields = new URLSearchParams();
  const digestFields = [];
  const values_summary = [];
  function append(control, value) {
    const normalize = text => text.replace(/\r\n|\r|\n/g, '\r\n');
    fields.append(normalize(control.name), normalize(value));
    const name = control.name;
    const hidden = control.type === 'hidden' || control.hidden === true || control.checkVisibility?.() === false;
    const password = control.type === 'password';
    const tokenName = /csrf|xsrf|token|nonce|authenticity/i.test(name);
    const credentialName = tokenName || /password|passwd|passphrase|secret|session|api.?key/i.test(name);
    const characters = Array.from(value);
    const length = characters.length;
    const frequencies = new Map();
    for (const character of characters) {
      frequencies.set(character, (frequencies.get(character) ?? 0) + 1);
    }
    const tokenLike = /^[a-z0-9_.+\/=-]{16,}$/i.test(value)
      && Array.from(frequencies.values()).reduce((entropy, count) => {
        const probability = count / length;
        return entropy - probability * Math.log2(probability);
      }, 0) >= 3.5;
    const excluded = control.type === 'hidden' && tokenName && tokenLike;
    digestFields.push([normalize(name), excluded ? null : normalize(value)]);
    const masked = password || (length > 32 && (!hidden || (credentialName && tokenLike)));
    const summary = password ? { length }
      : masked ? { prefix: characters.slice(0, 12).join(''), suffix: characters.slice(-4).join(''), length }
      : { value };
    values_summary.push({ name, hidden, ...summary, ...(excluded ? { excluded: true } : {}) });
  }
  let submitter;
  // Reading the controls directly avoids firing the page's formdata event.
  for (const control of form.elements) {
    if (control.matches(':disabled')) continue;
    if (control.type === 'submit' || control.type === 'image') {
      if (submitter || control.type === 'image' ||
          ['formaction', 'formmethod', 'formenctype', 'formtarget'].some(name => control.hasAttribute(name))) {
        return refused('expected at most one submit button without submission overrides');
      }
      submitter = control;
      if (control.name) append(control, control.value);
      continue;
    }
    if (!control.name || control instanceof HTMLButtonElement) continue;
    if (control instanceof HTMLInputElement) {
      if (['submit', 'reset', 'button', 'image'].includes(control.type)) continue;
      if (control.type === 'file') return refused('unsupported form control');
      if (['checkbox', 'radio'].includes(control.type) && !control.checked) continue;
      append(control, control.type === 'hidden' && control.name === '_charset_' ? 'UTF-8' : control.value);
    } else if (control instanceof HTMLTextAreaElement) {
      append(control, control.value);
    } else if (control instanceof HTMLSelectElement) {
      for (const option of control.selectedOptions) {
        if (!option.matches(':disabled')) append(control, option.value);
      }
    } else {
      return refused('unsupported form control');
    }
  }
  const body = fields.toString();
  if (body.length > 65_536) return refused('form payload is too large');
  const page = location.href;
  async function formDigest(payload) {
    const snapshot = new TextEncoder().encode(JSON.stringify([page, endpoint.href, payload]));
    const digest = await crypto.subtle.digest('SHA-256', snapshot);
    return Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, '0')).join('');
  }
  let hex = await formDigest(digestFields);
  // Approvals captured before token exclusion bind the entire encoded body.
  if (field_values_digest && hex !== field_values_digest && await formDigest(body) === field_values_digest) {
    hex = field_values_digest;
  }
  if (inspect) {
    if (new TextEncoder().encode(JSON.stringify(values_summary)).length > 256 * 1024) {
      return refused('form summary is too large');
    }
    return { ok: true, digest: hex, page_url: page, values_summary };
  }
  if (hex !== field_values_digest) return refused('form digest mismatch');
  if (location.href !== page || !(Date.now() - Date.parse(decided_at) < 300_000)) {
    return refused('approval expired or document location changed');
  }
  // Send the captured payload so page scripts cannot race the check by editing the form.
  try {
    const response = await fetch(endpoint.href, {
      method: 'POST', body: fields, credentials: 'same-origin', redirect: 'manual',
    });
    return { ok: true, status: response.status };
  } catch {
    return { ok: true, status: null };
  }
}
