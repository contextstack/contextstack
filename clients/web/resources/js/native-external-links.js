function bindNativeExternalLinks() {
    const root = document.documentElement;

    if (!root.hasAttribute('data-native-desktop')) {
        return;
    }

    const endpoint = root.dataset.nativeOpenExternalUrl;
    const csrf = document.querySelector('meta[name="csrf-token"]')?.content;

    if (!endpoint) {
        return;
    }

    document.addEventListener('click', (event) => {
        const target = event.target;

        if (!(target instanceof Element)) {
            return;
        }

        const anchor = target.closest('a[data-native-external-link]');

        if (!anchor) {
            return;
        }

        event.preventDefault();

        fetch(endpoint, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                Accept: 'application/json',
                'X-CSRF-TOKEN': csrf ?? '',
                'X-Requested-With': 'XMLHttpRequest',
            },
            body: JSON.stringify({ url: anchor.href }),
        });
    });
}

bindNativeExternalLinks();
