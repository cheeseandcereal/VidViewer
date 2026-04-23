// Collection page: Random button, rename/delete (for custom collections).

(() => {
    const grid = document.querySelector('.video-grid');
    const cid = grid?.dataset.collectionId;
    const randomBtn = document.getElementById('btn-random');
    const renameBtn = document.getElementById('btn-rename');
    const deleteBtn = document.getElementById('btn-delete');

    async function goRandom() {
        if (!cid) return;
        const resp = await fetch(`/api/collections/${encodeURIComponent(cid)}/random`);
        if (resp.status === 404) {
            alert('No playable videos in this collection.');
            return;
        }
        if (!resp.ok) {
            alert('Random failed');
            return;
        }
        const { video_id } = await resp.json();
        window.location.href = `/videos/${encodeURIComponent(video_id)}?cid=${encodeURIComponent(cid)}`;
    }

    if (randomBtn) randomBtn.addEventListener('click', goRandom);

    // 'R' keyboard shortcut — skip when focus is in text inputs.
    document.addEventListener('keydown', ev => {
        if (ev.defaultPrevented) return;
        if (ev.key !== 'r' && ev.key !== 'R') return;
        const t = ev.target;
        if (t instanceof HTMLElement) {
            const tag = t.tagName;
            if (tag === 'INPUT' || tag === 'TEXTAREA' || t.isContentEditable) return;
        }
        ev.preventDefault();
        goRandom();
    });

    if (renameBtn) renameBtn.addEventListener('click', async () => {
        const cur = document.querySelector('.collection-header h1').textContent.trim();
        const next = prompt('New name:', cur);
        if (!next || next === cur) return;
        const resp = await fetch(`/api/collections/${encodeURIComponent(cid)}`, {
            method: 'PATCH',
            headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ name: next.trim() }),
        });
        if (!resp.ok) {
            alert('Rename failed');
            return;
        }
        window.location.reload();
    });

    if (deleteBtn) deleteBtn.addEventListener('click', async () => {
        if (!confirm('Delete this collection? Videos themselves are not deleted.')) return;
        const resp = await fetch(`/api/collections/${encodeURIComponent(cid)}`, { method: 'DELETE' });
        if (!resp.ok) {
            alert('Delete failed');
            return;
        }
        window.location.href = '/';
    });
})();
