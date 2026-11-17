// Director times arrive as UTC ISO strings. <x-client.time> renders them inside <time data-local>, and this shows them
// in the viewer's own time zone. Livewire's morph puts the server's text back on every update, so they are
// formatted again after each morph and each wire:navigate.
const formatter = new Intl.DateTimeFormat([], { dateStyle: 'medium', timeStyle: 'short' });

function formatLocalTimes(root = document) {
    root.querySelectorAll('time[data-local]').forEach((el) => {
        const at = new Date(el.dateTime);
        if (!Number.isNaN(at.getTime())) {
            el.textContent = formatter.format(at);
        }
    });
}

document.addEventListener('DOMContentLoaded', () => formatLocalTimes());
document.addEventListener('livewire:navigated', () => formatLocalTimes());
document.addEventListener('livewire:init', () => {
    window.Livewire.hook('morphed', ({ el }) => formatLocalTimes(el));
});
