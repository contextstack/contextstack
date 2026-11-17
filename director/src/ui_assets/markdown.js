'use strict';

// Decode URL character references before checking the scheme. Unknown named
// references remain literal and cannot turn a refused scheme into an allowed one.
function decodedHref(raw) {
  const names = { amp: '&', colon: ':', Tab: '\t', NewLine: '\n', quot: '"', apos: "'", lt: '<', gt: '>' };
  return String(raw).replace(/&#(x[0-9a-f]+|[0-9]+);?|&(amp|colon|Tab|NewLine|quot|apos|lt|gt);/gi,
    (entity, number, name) => {
      if (name) return names[name] ?? entity;
      const code = number[0].toLowerCase() === 'x' ? parseInt(number.slice(1), 16) : parseInt(number, 10);
      return code > 0 && code <= 0x10ffff ? String.fromCodePoint(code) : '\ufffd';
    });
}

function allowedHref(raw) {
  const url = decodedHref(raw).trim();
  const normalized = url.toLowerCase();
  if (/^https?:\/\//.test(normalized) || /^mailto:/.test(normalized)) return url;
  return null;
}

function element(tag, text) {
  const node = document.createElement(tag);
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

/** Append inline Markdown into `parent` (uses text nodes for all source text). */
function inlineMarkdown(parent, source) {
  const parts = source.split('`');
  for (let i = 0; i < parts.length; i += 1) {
    if (i % 2 === 1) {
      parent.appendChild(element('code', parts[i]));
      continue;
    }
    parseText(parent, parts[i]);
  }
}

function parseText(parent, chunk) {
  let rest = chunk;
  while (rest.length) {
    const link = rest.match(/\[([^\]]+)\]\(([^)]+)\)/);
    if (link && link.index !== undefined) {
      if (link.index > 0) emitEmphasis(parent, rest.slice(0, link.index));
      const href = allowedHref(link[2]);
      if (href) {
        const a = element('a', link[1]);
        a.setAttribute('href', href);
        a.setAttribute('rel', 'noopener noreferrer');
        parent.appendChild(a);
      } else {
        parent.appendChild(document.createTextNode(link[1]));
      }
      rest = rest.slice(link.index + link[0].length);
      continue;
    }
    emitEmphasis(parent, rest);
    break;
  }
}

function emitEmphasis(parent, text) {
  let rest = text;
  while (rest.length) {
    const bold = rest.match(/(\*\*|__)(.+?)\1/);
    // Whichever starts first wins; a single-marker italic never matches inside a `**` run.
    const italic = rest.match(/(?<!\*)\*([^*]+)\*(?!\*)|(?<!_)_([^_]+)_(?!_)/);
    let pick = null;
    let kind = '';
    if (bold && (italic === null || bold.index <= italic.index)) {
      pick = bold;
      kind = 'strong';
    } else if (italic && italic.index !== undefined) {
      pick = italic;
      kind = 'em';
    }
    if (!pick || pick.index === undefined) {
      parent.appendChild(document.createTextNode(rest));
      return;
    }
    if (pick.index > 0) {
      parent.appendChild(document.createTextNode(rest.slice(0, pick.index)));
    }
    // bold: (marker)(content); italic: (*content*) | (_content_)
    const inner = kind === 'strong' ? pick[2] : (pick[1] ?? pick[2] ?? '');
    const wrap = element(kind);
    inlineMarkdown(wrap, inner);
    parent.appendChild(wrap);
    rest = rest.slice(pick.index + pick[0].length);
  }
}

function isBlank(line) {
  return !line.trim();
}

function listMarker(line) {
  const ul = line.match(/^(\s*)([-*+])\s+(.*)$/);
  if (ul) return { ordered: false, body: ul[3] };
  const ol = line.match(/^(\s*)(\d+)\.\s+(.*)$/);
  if (ol) return { ordered: true, body: ol[3] };
  return null;
}

/** Render assistant Markdown into a document fragment (text nodes and safe links only). */
function renderMarkdown(markdown) {
  const source = String(markdown ?? '');
  const lines = source.split('\n');
  const fragment = document.createDocumentFragment();
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    if (line.startsWith('```')) {
      const fence = line.slice(3).trim();
      const code = [];
      i += 1;
      while (i < lines.length && !lines[i].startsWith('```')) {
        code.push(lines[i]);
        i += 1;
      }
      if (i < lines.length) i += 1;
      const pre = element('pre');
      const codeEl = element('code', code.join('\n'));
      if (fence) codeEl.className = 'language-' + fence;
      pre.appendChild(codeEl);
      fragment.appendChild(pre);
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      const level = Math.min(heading[1].length, 6);
      const h = element('h' + level);
      inlineMarkdown(h, heading[2]);
      fragment.appendChild(h);
      i += 1;
      continue;
    }

    if (line.startsWith('>')) {
      const quoted = [];
      while (i < lines.length && lines[i].startsWith('>')) {
        quoted.push(lines[i].replace(/^>\s?/, ''));
        i += 1;
      }
      const block = element('blockquote');
      const inner = renderMarkdown(quoted.join('\n'));
      block.appendChild(inner);
      fragment.appendChild(block);
      continue;
    }

    const firstList = listMarker(line);
    if (firstList) {
      const items = [];
      let ordered = firstList.ordered;
      while (i < lines.length) {
        const marker = listMarker(lines[i]);
        if (!marker) break;
        ordered = marker.ordered;
        items.push(marker.body);
        i += 1;
      }
      const list = element(ordered ? 'ol' : 'ul');
      for (const body of items) {
        const li = element('li');
        inlineMarkdown(li, body);
        list.appendChild(li);
      }
      fragment.appendChild(list);
      continue;
    }

    if (isBlank(line)) {
      i += 1;
      continue;
    }

    const para = [];
    while (i < lines.length && !isBlank(lines[i]) && !lines[i].startsWith('```')
      && !lines[i].match(/^#{1,6}\s+/) && !lines[i].startsWith('>') && !listMarker(lines[i])) {
      para.push(lines[i]);
      i += 1;
    }
    const p = element('p');
    inlineMarkdown(p, para.join('\n'));
    fragment.appendChild(p);
  }
  return fragment;
}

globalThis.renderMarkdown = renderMarkdown;
