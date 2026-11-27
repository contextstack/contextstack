const statusLine = document.getElementById('status');
const list = document.getElementById('origins');
const button = document.getElementById('grant');
let pending = [];
let requesting = false;

async function refresh() {
  if (requesting) return;
  pending = await chrome.runtime.sendMessage({ type: 'pending_permissions' }) ?? [];
  if (requesting) return;
  list.replaceChildren(...pending.map(pattern => {
    const item = document.createElement('li');
    item.textContent = pattern === 'contentSettings' ? 'Temporarily block popups from the requested sites'
      : pattern === '<all_urls>' ? 'All sites for screenshots. Access is removed after capture.' : pattern;
    return item;
  }));
  button.hidden = pending.length === 0;
  statusLine.textContent = pending.length ? 'A browser request needs access to these sites.'
    : 'No browser request is waiting for site access.';
}

button.addEventListener('click', async () => {
  if (!pending.length || requesting) return;
  requesting = true;
  button.disabled = true;
  try {
    const origins = pending.filter(item => item !== 'contentSettings');
    const permissions = pending.filter(item => item === 'contentSettings');
    if (await chrome.permissions.request({ origins, permissions })) window.close();
    else statusLine.textContent = 'Site access was declined.';
  } catch {
    statusLine.textContent = 'Chrome could not grant site access.';
  } finally {
    requesting = false;
    button.disabled = false;
  }
});

void refresh();
setInterval(refresh, 1000);
