// History page: per-row "Clear" button.

(() => {
    document.addEventListener('click', async ev => {
        const target = ev.target;
        if (!(target instanceof HTMLElement)) return;
        if (!target.classList.contains('btn-clear-one')) return;
        const row = target.closest('.history-item');
        if (!row) return;
        const id = row.dataset.videoId;
        if (!id) return;
        if (!confirm('Clear this history entry?')) return;
        const resp = await fetch(`/api/history/${encodeURIComponent(id)}`, { method: 'DELETE' });
        if (!resp.ok) {
            alert('Clear failed');
            return;
        }
        row.remove();
    });
})();
