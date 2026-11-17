/**
 * Copy text to the clipboard: true when it was copied. navigator.clipboard exists only on a secure origin (https or
 * localhost), so a phone opening the app over plain http on the LAN falls back to a hidden text area and the
 * browser's copy command.
 */
export async function copyText(text, doc = globalThis.document, clipboard = globalThis.navigator?.clipboard) {
    if (clipboard?.writeText) {
        try {
            await clipboard.writeText(text);

            return true;
        } catch (error) {
            // Refused (no focus, no permission): try the fallback.
        }
    }
    if (!doc?.body) {
        return false;
    }
    const area = doc.createElement('textarea');
    area.value = text;
    area.setAttribute('readonly', '');
    area.style.position = 'fixed';
    area.style.opacity = '0';
    doc.body.appendChild(area);
    area.select();
    let copied = false;
    try {
        copied = doc.execCommand('copy');
    } catch (error) {
        copied = false;
    }
    area.remove();

    return copied;
}

if (typeof window !== 'undefined') {
    window.csCopy = copyText;
}
